use std::{
    collections::HashMap, fmt::Debug, hash::Hash, os::unix::prelude::FromRawFd, pin::Pin,
    process::Stdio, sync::Arc,
};

use anyhow::Result;
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use thiserror::Error;
use tokio::{
    fs::File,
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::{
        mpsc::{self, Receiver},
        RwLock,
    },
    task::JoinHandle,
};

use crate::{client::P9cpuCommand, rpc, AsBytes, FromVecu8};
#[async_trait]
pub trait P9cpuServerT {
    async fn serve(&self, net: &str, address: &str) -> Result<()>;
}

#[derive(Debug)]
enum StdinWriter {
    Piped(ChildStdin),
    Pty(tokio::fs::File),
}

impl AsyncWrite for StdinWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        match &mut *self {
            StdinWriter::Piped(inner) => Pin::new(inner).poll_write(cx, buf),
            StdinWriter::Pty(inner) => Pin::new(inner).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        match &mut *self {
            StdinWriter::Piped(inner) => Pin::new(inner).poll_flush(cx),
            StdinWriter::Pty(inner) => Pin::new(inner).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        match &mut *self {
            StdinWriter::Piped(inner) => Pin::new(inner).poll_shutdown(cx),
            StdinWriter::Pty(inner) => Pin::new(inner).poll_shutdown(cx),
        }
    }
}

#[derive(Debug)]
enum StdoutReader {
    Piped(ChildStdout),
    Pty(tokio::fs::File),
}

impl AsyncRead for StdoutReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            StdoutReader::Piped(inner) => Pin::new(inner).poll_read(cx, buf),
            StdoutReader::Pty(inner) => Pin::new(inner).poll_read(cx, buf),
        }
    }
}

#[derive(Debug)]
pub struct Session {
    stdin: Arc<RwLock<StdinWriter>>,
    stdout: Arc<RwLock<StdoutReader>>,
    stderr: Arc<RwLock<Option<ChildStderr>>>,
    child: Arc<RwLock<Child>>,
    handles: Arc<RwLock<Vec<JoinHandle<()>>>>,
}

#[derive(Error, Debug)]
pub enum P9cpuServerError {
    #[error("Failed to spawn: {0}")]
    SpawnFail(std::io::Error),
    #[error("Session does not exist")]
    SessionNotExist,
    #[error("Stdio: {0}")]
    StdIo(std::io::Error),
    #[error("Duplicate session id")]
    DuplicateId,
    #[error("Command exited without return code.")]
    NoReturnCode,
    #[error("Command error: {0}")]
    CommandError(std::io::Error),
    #[error("Cannot open pty device")]
    OpenPtyFail(nix::Error),
    #[error("Cannot clone file descriptor: {0}")]
    FdCloneFail(std::io::Error),
}

#[derive(Debug, Default)]
pub struct P9cpuServerInner<I> {
    sessions: Arc<RwLock<HashMap<I, Session>>>,
}

impl<SID> P9cpuServerInner<SID>
where
    SID: Eq + Hash + Debug,
{
    async fn get_session<O, R>(&self, sid: &SID, op: O) -> Result<R, P9cpuServerError>
    where
        O: Fn(&Session) -> R,
    {
        let sessions = self.sessions.read().await;
        let info = sessions.get(sid).ok_or(P9cpuServerError::SessionNotExist)?;
        Ok(op(info))
    }

    fn make_cmd(&self, command: P9cpuCommand) -> Result<(Command, Option<File>), P9cpuServerError> {
        let mut cmd = Command::new(command.program);
        cmd.args(command.args);
        let pty_master = if command.tty {
            let result =
                nix::pty::openpty(None, None).map_err(|e| P9cpuServerError::OpenPtyFail(e))?;
            let stdin = unsafe { std::fs::File::from_raw_fd(result.slave) };
            let stdout = stdin
                .try_clone()
                .map_err(|e| P9cpuServerError::FdCloneFail(e))?;
            let stderr = stdin
                .try_clone()
                .map_err(|e| P9cpuServerError::FdCloneFail(e))?;
            // Stdio::
            cmd.stdin(stdin).stdout(stdout).stderr(stderr);
            unsafe {
                cmd.pre_exec(|| {
                    close_fds::set_fds_cloexec(3, &[]);
                    nix::unistd::setsid()?;
                    nix::ioctl_none_bad!(tiocsctty, libc::TIOCSCTTY);
                    tiocsctty(0)?;
                    Ok(())
                });
            }
            Some(unsafe { tokio::fs::File::from_raw_fd(result.master) })
        } else {
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            None
        };
        Ok((cmd, pty_master))
    }

    pub async fn start(&self, command: P9cpuCommand, sid: SID) -> Result<(), P9cpuServerError> {
        let mut sessions = self.sessions.write().await;
        if sessions.contains_key(&sid) {
            return Err(P9cpuServerError::DuplicateId);
        }
        let (mut cmd, pty_master) = self.make_cmd(command)?;
        let mut child = cmd.spawn().map_err(P9cpuServerError::SpawnFail)?;
        let (stdin, stdout, stderr) = if let Some(master) = pty_master {
            let c = master.try_clone().await.unwrap();
            (StdinWriter::Pty(master), StdoutReader::Pty(c), None)
        } else {
            let stdin = child.stdin.take().unwrap();
            let stdout = child.stdout.take().unwrap();
            let stderr = child.stderr.take();
            (
                StdinWriter::Piped(stdin),
                StdoutReader::Piped(stdout),
                stderr,
            )
        };
        let info = Session {
            stdin: Arc::new(RwLock::new(stdin)),
            stdout: Arc::new(RwLock::new(stdout)),
            stderr: Arc::new(RwLock::new(stderr)),
            child: Arc::new(RwLock::new(child)),
            handles: Arc::new(RwLock::new(Vec::new())),
        };
        sessions.insert(sid, info);
        Ok(())
    }

    pub async fn stdin<'a, IS, Item>(
        &self,
        sid: &SID,
        mut in_stream: IS,
    ) -> Result<(), P9cpuServerError>
    where
        IS: Stream<Item = Item> + Unpin,
        Item: AsBytes<'a>,
    {
        let cmd_stdin = self.get_session(sid, |s| s.stdin.clone()).await?;
        let mut cmd_stdin = cmd_stdin.write().await;
        while let Some(item) = in_stream.next().await {
            cmd_stdin
                .write_all(item.as_bytes())
                .await
                .map_err(P9cpuServerError::StdIo)?;
        }
        Ok(())
    }

    pub async fn stdout<Item>(&self, sid: &SID) -> Result<Receiver<Item>, P9cpuServerError>
    where
        Item: FromVecu8 + Debug + Send + Sync + 'static,
    {
        let cmd_stdout = self.get_session(sid, |s| s.stdout.clone()).await?;
        let (tx, rx) = mpsc::channel(1);
        let out_handle = tokio::spawn(async move {
            let mut out = cmd_stdout.write().await;
            if let Err(_e) = send_buf(&mut *out, tx).await {}
        });
        let handles = self.get_session(sid, |s| s.handles.clone()).await?;
        handles.write().await.push(out_handle);
        Ok(rx)
    }

    pub async fn stderr<Item>(&self, sid: &SID) -> Result<Receiver<Item>, P9cpuServerError>
    where
        Item: FromVecu8 + Debug + Send + Sync + 'static,
    {
        let cmd_stderr = self.get_session(sid, |s| s.stderr.clone()).await?;
        let (tx, rx) = mpsc::channel(1);
        let err_handle = tokio::spawn(async move {
            let mut err = cmd_stderr.write().await;
            let Some(ref mut err) = &mut *err else {
                return;
            };
            if let Err(_e) = send_buf(&mut *err, tx).await {}
        });
        let handles = self.get_session(sid, |s| s.handles.clone()).await?;
        handles.write().await.push(err_handle);
        Ok(rx)
    }

    pub async fn wait(&self, sid: &SID) -> Result<i32, P9cpuServerError> {
        let child = self.get_session(sid, |s| s.child.clone()).await?;
        let ret = match child.write().await.wait().await {
            Ok(status) => status.code().ok_or(P9cpuServerError::NoReturnCode),
            Err(e) => Err(P9cpuServerError::CommandError(e)),
        };
        let handles = self.get_session(sid, |s| s.handles.clone()).await?;
        for handle in handles.write().await.iter_mut() {
            if let Err(e) = handle.await {
                eprintln!("handle join error {:?}", e);
            }
        }
        self.sessions.write().await.remove(sid);
        ret
    }
}

async fn send_buf<R, Item>(src: &mut R, tx: mpsc::Sender<Item>) -> Result<()>
where
    R: AsyncRead + Unpin,
    Item: Sync + Send + Debug + 'static + FromVecu8,
{
    loop {
        let mut buf = vec![0; 1];
        if src.read(&mut buf).await? == 0 {
            break;
        }
        tx.send(Item::from_vec_u8(buf)).await?;
    }
    Ok(())
}

pub fn rpc_based() -> rpc::rpc_server::RpcServer {
    rpc::rpc_server::RpcServer {}
}
