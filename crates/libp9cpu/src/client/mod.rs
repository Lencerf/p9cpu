use std::fmt::Debug;
use std::path::Path;
use std::pin::Pin;
use std::vec;

use crate::rpc;
use crate::Addr;
use crate::cmd::{Command, CommandReq};
use anyhow::Result;
use async_trait::async_trait;
use futures::Future;

use futures::{Stream, StreamExt};
use thiserror::Error;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::broadcast;
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_stream::wrappers::ReceiverStream;

#[async_trait]
pub trait ClientInnerT2 {
    type Error: std::error::Error + Sync + Send + 'static;
    type SessionId: Clone + Debug + Sync + Send + 'static;

    async fn dial(&mut self) -> Result<Self::SessionId, Self::Error>;

    async fn start(
        &mut self,
        sid: Self::SessionId,
        command: CommandReq,
    ) -> Result<(), Self::Error>;

    // type ByteStream: Stream<Item = Result<u8, Self::Error>> + Unpin + Send + 'static;
    // async fn ttyout(&mut self, sid: Self::SessionId) -> Result<Self::ByteStream, Self::Error>;

    type EmptyFuture: Future<Output = Result<(), Self::Error>> + Send + 'static;
    // async fn ttyin(
    //     &mut self,
    //     sid: Self::SessionId,
    //     stream: impl Stream<Item = u8> + Send + Sync + 'static + Unpin,
    // ) -> Self::EmptyFuture;

    type ByteVecStream: Stream<Item = Vec<u8>> + Unpin + Send + 'static;
    async fn stdout(&mut self, sid: Self::SessionId) -> Result<Self::ByteVecStream, Self::Error>;
    async fn stderr(&mut self, sid: Self::SessionId) -> Result<Self::ByteVecStream, Self::Error>;

    async fn stdin(
        &mut self,
        sid: Self::SessionId,
        stream: impl Stream<Item = Vec<u8>> + Send + Sync + 'static + Unpin,
    ) -> Self::EmptyFuture;

    async fn ninep_forward(
        &mut self,
        sid: Self::SessionId,
        stream: impl Stream<Item = Vec<u8>> + Send + Sync + 'static + Unpin,
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
//         command: Command,
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
    Item: Into<Vec<u8>>,
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
                        self.buffer = item.into();
                        self.consumed = 0;
                        if self.buffer.is_empty() {
                            return std::task::Poll::Ready(Ok(()));
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
    handles: Vec<JoinHandle<Result<(), P9cpuClientError>>>,
    stop_tx: broadcast::Sender<()>,
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
    #[error("IO Error {0}")]
    IoErr(#[from] std::io::Error),
    #[error("Inner {0}")]
    Inner(Box<dyn std::error::Error + Sync + Send + 'static>),
    #[error("Channel closed.")]
    ChannelClosed,
}

impl<T> From<mpsc::error::SendError<T>> for P9cpuClientError {
    fn from(_: mpsc::error::SendError<T>) -> Self {
        P9cpuClientError::ChannelClosed
    }
}

pub struct P9cpuClient<Inner: ClientInnerT2> {
    inner: Inner,
    session_info: Option<SessionInfo<Inner::SessionId>>,
}

impl<'a, Inner> P9cpuClient<Inner>
where
    Inner: ClientInnerT2,
    std::io::Error: From<<Inner as ClientInnerT2>::Error>,
    P9cpuClientError: From<Inner::Error>,
{
    pub async fn new(inner: Inner) -> Result<P9cpuClient<Inner>> {
        Ok(Self {
            inner,
            session_info: None,
        })
    }

    const STDIN_BUF_SIZE: usize = 128;

    async fn setup_stdio(
        &mut self,
        sid: Inner::SessionId,
        tty: bool,
        mut stop_rx: broadcast::Receiver<()>,
    ) -> Result<Vec<JoinHandle<Result<(), P9cpuClientError>>>, Inner::Error> {
        let mut handles = vec![];

        let out_stream = self.inner.stdout(sid.clone()).await?;
        let stdout = tokio::io::stdout();
        let out_handle = Self::copy_stream(out_stream, stdout, true);
        handles.push(out_handle);

        if !tty {
            let err_stream = self.inner.stderr(sid.clone()).await?;
            let stderr = tokio::io::stderr();
            let err_handle = Self::copy_stream(err_stream, stderr, true);
            handles.push(err_handle);
        }

        let (tx, rx) = mpsc::channel(1);
        // let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        let in_stream = ReceiverStream::new(rx);
        let stdin_future = self.inner.stdin(sid.clone(), in_stream).await;
        let in_handle = tokio::spawn(async move {
            let mut stdin = tokio::io::stdin();
            loop {
                let mut buf = vec![0; Self::STDIN_BUF_SIZE];
                let len = tokio::select! {
                    len = stdin.read(&mut buf) => len,
                    _ = stop_rx.recv() => break,
                }?;
                if len == 0 {
                    break;
                }
                buf.truncate(len);
                tx.send(buf).await?;
            }
            drop(tx);
            stdin_future.await?;
            Ok(())
        });
        handles.push(in_handle);
        Ok(handles)
    }

    fn copy_stream<D>(
        mut src: Inner::ByteVecStream,
        mut dst: D,
        flush: bool,
    ) -> JoinHandle<Result<(), P9cpuClientError>>
    where
        D: AsyncWrite + Unpin + Send + 'static,
    {
        
        tokio::spawn(async move {
            while let Some(bytes) = src.next().await {
                dst.write_all(&bytes).await?;
                if flush {
                    dst.flush().await?;
                }
            }
            Ok(())
        })
    }

    // async fn setup_tty(
    //     &mut self,
    //     sid: Inner::SessionId,
    //     mut stop_rx: broadcast::Receiver<()>,
    // ) -> Result<Vec<JoinHandle<Result<(), P9cpuClientError>>>, Inner::Error> {
    //     // let mut out_stream = self.inner.ttyout(sid.clone()).await?;
    //     // let mut stdout = tokio::io::stdout();
    //     // let out_handle = tokio::spawn(async move {
    //     //     while let Some(item) = out_stream.next().await {
    //     //         let byte = item?;
    //     //         stdout.write_u8(byte).await?;
    //     //         stdout.flush().await?;
    //     //     }
    //     //     std::io::Result::Ok(())
    //     // });

    //     let out_stream = self.inner.stdout(sid.clone()).await?;
    //     let stdout = tokio::io::stdout();
    //     let out_handle = Self::copy_stream(out_stream, stdout, true);

    //     let (tx, rx) = mpsc::channel(1);
    //     let in_stream = ReceiverStream::new(rx);
    //     let ttyin_future = self.inner.ttyin(sid.clone(), in_stream).await;
    //     let in_handle = tokio::spawn(async move {
    //         let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
    //         loop {
    //             let mut buf = [0];
    //             // let a = stdin.read_u8().await;
    //             let len = tokio::select! {
    //                 len = stdin.read(&mut buf) => len,
    //                 _ = stop_rx.recv() => break,
    //             }?;
    //             if len == 0 {
    //                 break;
    //             }
    //             tx.send(buf[0].into()).await?;
    //         }
    //         drop(tx);
    //         ttyin_future.await?;
    //         Ok(())
    //     });

    //     Ok(vec![out_handle, in_handle])
    // }

    pub async fn start(&mut self, command: Command) -> Result<(), P9cpuClientError> {
        if self.session_info.is_some() {
            return Err(P9cpuClientError::AlreadyStarted)?;
        }
        let tty = command.req.tty;
        let sid = self.inner.dial().await?;
        if command.req.ninep {
            let (ninep_tx, ninep_rx) = mpsc::channel(1);
            // ninep_tx.send(<Inner as ClientInnerT>::NinepInStreamItem::from(vec![])).await;
            let ninep_in_stream = ReceiverStream::from(ninep_rx);
            let ninep_out_stream = self
                .inner
                .ninep_forward(sid.clone(), ninep_in_stream)
                .await?;
            println!("ninep forward established");

            // let reader = ninep_out_stream.map_err(|e| e.into()).into_async_read();

            let reader = StreamReader::new(ninep_out_stream);
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
        self.inner.start(sid.clone(), command.req).await?;
        println!("will set up stdio");
        let (stop_tx, stop_rx) = broadcast::channel(1);
        // let handles = if tty {
        //     self.setup_tty(sid.clone(), stop_rx).await
        // } else {
        //     self.setup_stdio(sid.clone(), stop_rx).await
        // }?;
        let handles = self.setup_stdio(sid.clone(), tty, stop_rx).await?;
        // let mut out_stream = self.inner.stdout(sid.clone()).await?;
        // let out_handle = tokio::spawn(async move {
        //     let mut stdout = tokio::io::stdout();
        //     while let Some(item) = out_stream.next().await {
        //         match item {
        //             Ok(bytes) => {
        //                 if stdout.write_all(&bytes).await.is_err() || stdout.flush().await.is_err()
        //                 {
        //                     break;
        //                 }
        //             }
        //             Err(e) => {
        //                 eprintln!("stdout {:?}", e);
        //                 break;
        //             }
        //         }
        //     }
        // });
        // let mut err_stream = self.inner.stderr(sid.clone()).await?;
        // let error_handle = tokio::spawn(async move {
        //     let mut stderr = tokio::io::stderr();
        //     while let Some(item) = err_stream.next().await {
        //         match item {
        //             Ok(bytes) => {
        //                 if stderr.write_all(&bytes).await.is_err() || stderr.flush().await.is_err()
        //                 {
        //                     break;
        //                 }
        //             }
        //             Err(e) => {
        //                 eprintln!("stderr {:?}", e);
        //                 break;
        //             }
        //         }
        //     }
        // });

        // let (tx, rx) = mpsc::channel(1);
        // let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        // let in_stream = ReceiverStream::new(rx);
        // let stdin_future = self.inner.stdin(sid.clone(), in_stream).await;
        // let in_handle = tokio::spawn(async move {
        //     loop {
        //         let mut buf = vec![0];
        //         let mut stdin = tokio::io::stdin();
        //         let Ok(len) = tokio::select! {
        //             len = stdin.read(&mut buf) => len,
        //             _ = &mut stop_rx => break,
        //         } else {
        //             break;
        //         };
        //         buf.truncate(len);
        //         if tx.send(buf.into()).await.is_err() {
        //             break;
        //         }
        //     }
        //     drop(tx);
        //     if let Err(e) = stdin_future.await {
        //         eprintln!("stdin future join error: {:?}", e);
        //     } else {
        //         eprintln!("stdin future done");
        //     }
        // });
        self.session_info = Some(SessionInfo {
            tty,
            sid,
            handles,
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
            match handle.await {
                Err(e) => eprintln!("thread join error: {:?}", e),
                Ok(Err(e)) => eprintln!("thread error {:?}", e),
                Ok(Ok(())) => {}
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
