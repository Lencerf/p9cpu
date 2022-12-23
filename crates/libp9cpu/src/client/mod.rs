use std::fmt::Debug;
use std::path::Path;
use std::pin::Pin;
use std::vec;

use crate::rpc;
use crate::Addr;
use crate::P9cpuCommand;
use crate::{AsBytes, IntoByteVec};
use anyhow::Result;
use async_trait::async_trait;
use futures::Future;
use futures::TryStreamExt;
use futures::{Stream, StreamExt};
use thiserror::Error;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_stream::wrappers::ReceiverStream;

#[async_trait]
pub trait ClientInnerT2 {
    type Error: std::error::Error + Sync + Send + Into<std::io::Error> + 'static;
    type SessionId: Clone + Debug + Sync + Send + 'static;

    async fn dial(&mut self) -> Result<Self::SessionId, Self::Error>;

    async fn start(
        &mut self,
        sid: Self::SessionId,
        command: P9cpuCommand,
    ) -> Result<(), Self::Error>;

    type ByteStream: Stream<Item = Result<u8, Self::Error>> + Unpin + 'static;
    async fn ttyout(&mut self, sid: Self::SessionId) -> Result<Self::ByteStream, Self::Error>;

    type EmptyFuture: Future<Output = Result<(), Self::Error>> + Send + 'static;
    type TtyinItem: Send + 'static + From<u8>;
    async fn ttyin(
        &mut self,
        sid: Self::SessionId,
        stream: impl Stream<Item = Self::TtyinItem> + Send + Sync + 'static + Unpin,
    ) -> Self::EmptyFuture;

    type ByteVecStream: Stream<Item = Result<Vec<u8>, Self::Error>> + Unpin + Send + 'static;
    async fn stdout(&mut self, sid: Self::SessionId) -> Result<Self::ByteVecStream, Self::Error>;
    async fn stderr(&mut self, sid: Self::SessionId) -> Result<Self::ByteVecStream, Self::Error>;

    type StdinItem: Send + 'static + From<Vec<u8>>;
    async fn stdin(
        &mut self,
        sid: Self::SessionId,
        stream: impl Stream<Item = Self::StdinItem> + Send + Sync + 'static + Unpin,
    ) -> Self::EmptyFuture;

    type NinepInStreamItem: Send + 'static + From<Vec<u8>>;
    async fn ninep_forward(
        &mut self,
        sid: Self::SessionId,
        stream: impl Stream<Item = Self::NinepInStreamItem> + Send + Sync + 'static + Unpin,
    ) -> Result<Self::ByteVecStream, Self::Error>;

    async fn wait(&mut self, sid: Self::SessionId) -> Result<i32, Self::Error>;
}

// #[async_trait]
// pub trait ClientInnerT {
//     type Error: Sync + Send + std::error::Error + 'static;
//     type SessionId: Clone + Debug + Sync + Send + 'static;
//     async fn dial(&mut self) -> Result<Self::SessionId, Self::Error>;
//     async fn start(
//         &mut self,
//         sid: Self::SessionId,
//         command: P9cpuCommand,
//     ) -> Result<(), Self::Error>;

//     async fn wait(&mut self, sid: Self::SessionId) -> Result<i32, Self::Error>;

//     type OutStream: Send + 'static + Stream + Unpin;
//     async fn stdout(&mut self, sid: Self::SessionId) -> Result<Self::OutStream, Self::Error>;
//     async fn stderr(&mut self, sid: Self::SessionId) -> Result<Self::OutStream, Self::Error>;

//     type InStreamItem: Send + 'static + From<Vec<u8>>;
//     type StdinFuture: Future<Output = Result<(), Self::Error>> + Send + 'static;
//     async fn stdin(
//         &mut self,
//         sid: Self::SessionId,
//         stream: impl Stream<Item = Self::InStreamItem> + Send + Sync + 'static + Unpin,
//     ) -> Self::StdinFuture;

//     type NinepInStreamItem: Send + 'static + From<Vec<u8>>;
//     type NinepOutStream: Send + 'static + Stream + Unpin;
//     async fn ninep_forward(
//         &mut self,
//         sid: Self::SessionId,
//         stream: impl Stream<Item = Self::NinepInStreamItem> + Send + Sync + 'static + Unpin,
//     ) -> Result<Self::NinepOutStream, Self::Error>;
// }

struct StreamReader<S> {
    inner: S,
    buffer: Vec<u8>,
    consumed: usize,
}

impl<S> StreamReader<S> {
    pub fn new(stream: S) -> Self {
        StreamReader {
            inner: stream,
            buffer: vec![],
            consumed: 0,
        }
    }
}

impl<'a, S, Item> AsyncRead for StreamReader<S>
where
    S: Stream<Item = Item> + Unpin,
    Item: Into<Result<Vec<u8>, std::io::Error>>,
{
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if buf.remaining() == 0 {
            return std::task::Poll::Ready(Ok(()));
        }
        loop {
            if self.consumed < self.buffer.len() {
                let remaining = self.buffer.len() - self.consumed;
                let read_to = std::cmp::min(buf.remaining(), remaining) + self.consumed;
                buf.put_slice(&self.buffer[self.consumed..read_to]);
                self.consumed = read_to;
                return std::task::Poll::Ready(Ok(()));
            } else {
                match Pin::new(&mut self.inner).poll_next(cx) {
                    std::task::Poll::Ready(Some(item)) => {
                        match item.into() {
                            Ok(buffer) => {
                                self.buffer = buffer;
                                self.consumed = 0;
                                if self.buffer.is_empty() {
                                    return std::task::Poll::Ready(Ok(()));
                                }
                            }
                            Err(e) => return std::task::Poll::Ready(Err(e.into())),
                         }
                    }
                    std::task::Poll::Ready(None) => {
                        return std::task::Poll::Ready(Ok(()));
                    }
                    std::task::Poll::Pending => return std::task::Poll::Pending,
                }
            }
        }
    }
}

struct SenderWriter<Item> {
    inner: Option<tokio_util::sync::PollSender<Item>>,
}

impl<Item> SenderWriter<Item>
where
    Item: Send + 'static,
{
    pub fn new(sender: mpsc::Sender<Item>) -> Self {
        Self {
            inner: Some(tokio_util::sync::PollSender::new(sender)),
        }
    }
}

impl<Item> AsyncWrite for SenderWriter<Item>
where
    Item: From<Vec<u8>> + Send + 'static,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        if buf.is_empty() {
            return std::task::Poll::Ready(Ok(0));
        }

        let Some(inner )= self.inner.as_mut() else {
            return std::task::Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "Sender is down.",
        )))};
        match inner.poll_reserve(cx) {
            std::task::Poll::Pending => return std::task::Poll::Pending,
            std::task::Poll::Ready(Ok(())) => {}
            std::task::Poll::Ready(Err(_)) => {
                return std::task::Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "Channel is closed.",
                )))
            }
        };
        let item = buf.to_vec().into();
        match inner.send_item(item) {
            Ok(()) => std::task::Poll::Ready(Ok(buf.len())),
            Err(_) => std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "Channel is closed.",
            ))),
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        if self.inner.is_some() {
            std::task::Poll::Ready(Ok(()))
        } else {
            std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "Sender is down.",
            )))
        }
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        // std::task::Poll::Ready(Ok(()))
        match self.inner.take() {
            Some(mut inner) => {
                inner.close();
                std::task::Poll::Ready(Ok(()))
            }
            None => std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "Sender is down.",
            ))),
        }
    }
}

struct SessionInfo<S> {
    sid: S,
    handles: Vec<JoinHandle<()>>,
    stop_tx: oneshot::Sender<()>,
    tty: bool,
}

#[derive(Error, Debug)]
pub enum P9cpuClientError {
    #[error("Command not started")]
    NotStarted,
    #[error("Command exits with {0}")]
    NonZeroExitCode(i32),
    #[error("Command already started")]
    AlreadyStarted,
}

pub struct P9cpuClient<Inner: ClientInnerT2> {
    inner: Inner,
    session_info: Option<SessionInfo<Inner::SessionId>>,
}

impl<'a, Inner> P9cpuClient<Inner>
where
    Inner: ClientInnerT2,
    // <<Inner as ClientInnerT>::OutStream as Stream>::Item: crate::AsBytes<'a> + Sync + Send,
    // <<Inner as ClientInnerT>::NinepOutStream as Stream>::Item: IntoByteVec,
{
    pub async fn new(inner: Inner) -> Result<P9cpuClient<Inner>> {
        Ok(Self {
            inner,
            session_info: None,
        })
    }

    pub async fn start(&mut self, command: P9cpuCommand) -> Result<()> {
        if self.session_info.is_some() {
            return Err(P9cpuClientError::AlreadyStarted)?;
        }
        let tty = command.tty;
        let sid = self.inner.dial().await?;
        if !command.namespace.is_empty() {
            let (ninep_tx, ninep_rx) = mpsc::channel(1);
            // ninep_tx.send(<Inner as ClientInnerT>::NinepInStreamItem::from(vec![])).await;
            let ninep_in_stream = ReceiverStream::from(ninep_rx);
            let ninep_out_stream = self
                .inner
                .ninep_forward(sid.clone(), ninep_in_stream)
                .await?;
            println!("ninep forward established");
            
            // let reader = ninep_out_stream.map_err(|e| e.into()).into_async_read();

            let reader = StreamReader::new(ninep_out_stream.map_err(|e| e.into()));
            let writer = SenderWriter::new(ninep_tx);
            // tokio_util::io::StreamReader::new(ninep_out_stream);
            let root = Path::new("/");
            tokio::spawn(async move {
                if let Err(e) =
                    rs9p::srv::dispatch(rs9p::unpfs::Unpfs::new(root), reader, writer).await
                {
                    println!("rs9p error : {:?}", e);
                }
            });
        }
        self.inner.start(sid.clone(), command).await?;
        let mut out_stream = self.inner.stdout(sid.clone()).await?;
        let out_handle = tokio::spawn(async move {
            let mut stdout = tokio::io::stdout();
            while let Some(item) = out_stream.next().await {
                match item {
                    Ok(bytes) => {
                        if stdout.write_all(&bytes).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("stdout {:?}", e);
                        break;
                    }
                }
            }
        });
        let mut err_stream = self.inner.stderr(sid.clone()).await?;
        let error_handle = tokio::spawn(async move {
            let mut stderr = tokio::io::stderr();
            while let Some(item) = err_stream.next().await {
                match item {
                    Ok(bytes) => {
                        if stderr.write_all(&bytes).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("stderr {:?}", e);
                        break;
                    }
                }
            }
        });

        let (tx, rx) = mpsc::channel(1);
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        let in_stream = ReceiverStream::new(rx);
        let stdin_future = self.inner.stdin(sid.clone(), in_stream).await;
        let in_handle = tokio::spawn(async move {
            loop {
                let mut buf = vec![0];
                let mut stdin = tokio::io::stdin();
                let Ok(len) = tokio::select! {
                    len = stdin.read(&mut buf) => len,
                    _ = &mut stop_rx => break,
                } else {
                    break;
                };
                buf.truncate(len);
                if tx.send(buf.into()).await.is_err() {
                    break;
                }
            }
            drop(tx);
            if let Err(e) = stdin_future.await {
                eprintln!("stdin future join error: {:?}", e);
            } else {
                eprintln!("stdin future done");
            }
        });
        self.session_info = Some(SessionInfo {
            tty,
            sid,
            handles: vec![out_handle, error_handle, in_handle],
            stop_tx,
        });
        Ok(())
    }

    pub async fn wait_inner(&mut self, sid: Inner::SessionId) -> Result<()> {
        let code = self.inner.wait(sid).await?;
        if code == 0 {
            Ok(())
        } else {
            Err(P9cpuClientError::NonZeroExitCode(code))?
        }
    }

    pub async fn wait(&mut self) -> Result<()> {
        let SessionInfo {
            sid,
            handles,
            stop_tx,
            tty,
        } = self
            .session_info
            .take()
            .ok_or(P9cpuClientError::NotStarted)?;
        let mut termios_attr = None;
        if tty {
            let current = nix::sys::termios::tcgetattr(0)?;
            let mut raw = current.clone();
            nix::sys::termios::cfmakeraw(&mut raw);
            nix::sys::termios::tcsetattr(0, nix::sys::termios::SetArg::TCSANOW, &raw)?;
            termios_attr = Some(current)
        }
        let ret = self.wait_inner(sid).await;
        if stop_tx.send(()).is_err() {
            eprintln!("stdin thread not working");
        }
        for handle in handles {
            if let Err(e) = handle.await {
                eprintln!("thread join error: {:?}", e);
            }
        }
        if let Some(current) = termios_attr {
            if let Err(e) =
                nix::sys::termios::tcsetattr(0, nix::sys::termios::SetArg::TCSANOW, &current)
            {
                eprintln!("resotre error: {:?}", e);
            }
        }
        ret
    }
}

pub async fn rpc_based(addr: Addr) -> Result<P9cpuClient<rpc::rpc_client::RpcInner>> {
    let inner = rpc::rpc_client::RpcInner::new(addr).await?;
    let client = P9cpuClient::new(inner).await?;
    Ok(client)
}
