use std::fmt::Debug;
use std::vec;

use crate::rpc;
use crate::AsBytes;
use anyhow::Result;
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_stream::wrappers::ReceiverStream;

pub type P9cpuCommand = crate::rpc::P9cpuStartRequest;

#[async_trait]
pub trait ClientInnerT {
    type Error;
    type SessionId;
    async fn start(&mut self, command: P9cpuCommand) -> Result<Self::SessionId, Self::Error>;

    async fn wait(&mut self, sid: Self::SessionId) -> Result<i32, Self::Error>;

    type OutStream;
    async fn stdout(&mut self, sid: Self::SessionId) -> Result<Self::OutStream, Self::Error>;
    async fn stderr(&mut self, sid: Self::SessionId) -> Result<Self::OutStream, Self::Error>;

    type InStreamItem;
    async fn stdin(
        &mut self,
        sid: Self::SessionId,
        stream: impl Stream<Item = Self::InStreamItem> + Send + Sync + 'static + Unpin,
    ) -> Result<(), Self::Error>;

    fn side_channel(&mut self) -> Self;
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

pub struct P9cpuClient<Inner: ClientInnerT> {
    inner: Inner,
    stdin_tx: mpsc::Sender<(
        <Inner as ClientInnerT>::SessionId,
        mpsc::Receiver<<Inner as ClientInnerT>::InStreamItem>,
    )>,
    session_info: Option<SessionInfo<Inner::SessionId>>,
}

impl<'a, Inner> P9cpuClient<Inner>
where
    Inner: ClientInnerT + Sync + Send + 'static,
    <Inner as ClientInnerT>::Error: Sync + Send + std::error::Error + 'static,
    <Inner as ClientInnerT>::OutStream: Send + 'static + Stream + Unpin,
    <<Inner as ClientInnerT>::OutStream as Stream>::Item: crate::AsBytes<'a> + Sync + Send,
    <Inner as ClientInnerT>::InStreamItem: Send + 'static + From<Vec<u8>>,
    <Inner as ClientInnerT>::SessionId: Clone + Debug + Sync + Send + 'static,
    // <<Inner as ClientInnerT>::InStream as Stream>::Item: Sync + Send + 'static,
{
    pub async fn new(mut inner: Inner) -> Result<P9cpuClient<Inner>> {
        let mut stdin_channel = inner.side_channel();
        let (stdin_tx, mut std_rx) =
            mpsc::channel::<(Inner::SessionId, mpsc::Receiver<Inner::InStreamItem>)>(1);
        tokio::spawn(async move {
            while let Some((sid, rx)) = std_rx.recv().await {
                let in_stream = ReceiverStream::new(rx);
                if let Err(_e) = stdin_channel.stdin(sid, in_stream).await {}
            }
        });
        Ok(Self {
            stdin_tx,
            inner,
            session_info: None,
        })
    }

    pub async fn start(&mut self, command: P9cpuCommand) -> Result<()> {
        if self.session_info.is_some() {
            return Err(P9cpuClientError::AlreadyStarted)?;
        }
        let tty = command.tty;
        let sid = self.inner.start(command).await?;
        let mut out_stream = self.inner.stdout(sid.clone()).await?;
        let out_handle = tokio::spawn(async move {
            let mut stdout = tokio::io::stdout();
            while let Some(item) = out_stream.next().await {
                if stdout.write_all(item.as_bytes()).await.is_err() || stdout.flush().await.is_err()
                {
                    break;
                }
            }
        });
        let mut err_stream = self.inner.stderr(sid.clone()).await?;
        let error_handle = tokio::spawn(async move {
            let mut stderr = tokio::io::stderr();
            while let Some(item) = err_stream.next().await {
                if stderr.write_all(item.as_bytes()).await.is_err() || stderr.flush().await.is_err()
                {
                    break;
                }
            }
        });
        let (tx, rx) = mpsc::channel(1);
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        self.stdin_tx.send((sid.clone(), rx)).await?;
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

pub async fn rpc_based(addr: &str) -> Result<P9cpuClient<rpc::rpc_client::RpcInner>> {
    let inner = rpc::rpc_client::RpcInner::new(addr).await?;
    let client = P9cpuClient::new(inner).await?;
    Ok(client)
}
