use std::{
    collections::HashMap,
    ffi::{CString, OsStr},
    fmt::{Debug, Display},
    hash::Hash,
    io::ErrorKind,
    os::unix::prelude::{AsRawFd, FromRawFd, OsStrExt, OwnedFd},
    pin::Pin,
    sync::Arc,
};

use crate::cmd::CommandReq;
use crate::{cmd::FsTab, rpc};
use anyhow::Result;
use async_trait::async_trait;
use futures::{ready, Stream, StreamExt};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::{Child, Command},
    sync::{mpsc, RwLock},
    task::JoinHandle,
};
use tokio_stream::wrappers::ReceiverStream;

#[async_trait]
pub trait P9cpuServerT {
    async fn serve(&self, addr: crate::Addr) -> Result<()>;
}

enum ChildStdio {
    Piped(OwnedFd, OwnedFd, OwnedFd),
    Pty(OwnedFd),
}

// #[derive(Debug)]
// enum StdinWriter {
//     Piped(ChildStdin),
//     Pty(tokio::fs::File),
// }

// impl AsyncWrite for StdinWriter {
//     fn poll_write(
//         mut self: Pin<&mut Self>,
//         cx: &mut std::task::Context<'_>,
//         buf: &[u8],
//     ) -> std::task::Poll<Result<usize, std::io::Error>> {
//         match &mut *self {
//             StdinWriter::Piped(inner) => Pin::new(inner).poll_write(cx, buf),
//             StdinWriter::Pty(inner) => Pin::new(inner).poll_write(cx, buf),
//         }
//     }

//     fn poll_flush(
//         mut self: Pin<&mut Self>,
//         cx: &mut std::task::Context<'_>,
//     ) -> std::task::Poll<Result<(), std::io::Error>> {
//         match &mut *self {
//             StdinWriter::Piped(inner) => Pin::new(inner).poll_flush(cx),
//             StdinWriter::Pty(inner) => Pin::new(inner).poll_flush(cx),
//         }
//     }

//     fn poll_shutdown(
//         mut self: Pin<&mut Self>,
//         cx: &mut std::task::Context<'_>,
//     ) -> std::task::Poll<Result<(), std::io::Error>> {
//         match &mut *self {
//             StdinWriter::Piped(inner) => Pin::new(inner).poll_shutdown(cx),
//             StdinWriter::Pty(inner) => Pin::new(inner).poll_shutdown(cx),
//         }
//     }
// }

// #[derive(Debug)]
// enum StdoutReader {
//     Piped(ChildStdout),
//     Pty(tokio::fs::File),
// }

// impl AsyncRead for StdoutReader {
//     fn poll_read(
//         mut self: Pin<&mut Self>,
//         cx: &mut std::task::Context<'_>,
//         buf: &mut tokio::io::ReadBuf<'_>,
//     ) -> std::task::Poll<std::io::Result<()>> {
//         match &mut *self {
//             StdoutReader::Piped(inner) => Pin::new(inner).poll_read(cx, buf),
//             StdoutReader::Pty(inner) => Pin::new(inner).poll_read(cx, buf),
//         }
//     }
// }
#[derive(Debug)]
pub struct StdioFd {
    fd: tokio::io::unix::AsyncFd<OwnedFd>,
}

impl TryFrom<OwnedFd> for StdioFd {
    type Error = std::io::Error;

    fn try_from(value: OwnedFd) -> Result<Self, Self::Error> {
        let flags = unsafe { libc::fcntl(value.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(std::io::Error::last_os_error());
        }

        let ret =
            unsafe { libc::fcntl(value.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
        if ret < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = tokio::io::unix::AsyncFd::new(value)?;
        Ok(Self { fd })
    }
}

impl AsyncRead for StdioFd {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        loop {
            let mut guard = ready!(self.fd.poll_read_ready(cx))?;
            let ret = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    buf.unfilled_mut() as *mut _ as _,
                    buf.remaining(),
                )
            };
            if ret < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == ErrorKind::WouldBlock {
                    guard.clear_ready();
                    continue;
                } else {
                    return std::task::Poll::Ready(Err(e));
                }
            } else {
                let n = ret as usize;
                unsafe { buf.assume_init(n) };
                buf.advance(n);
                return std::task::Poll::Ready(Ok(()));
            }
        }
    }
}

impl AsyncWrite for StdioFd {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        loop {
            let mut guard = ready!(self.fd.poll_write_ready(cx))?;
            let ret = unsafe { libc::write(self.fd.as_raw_fd(), buf as *const _ as _, buf.len()) };
            if ret < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == ErrorKind::WouldBlock {
                    guard.clear_ready();
                    continue;
                } else {
                    return std::task::Poll::Ready(Err(e));
                }
            } else {
                return std::task::Poll::Ready(Ok(ret as usize));
            }
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[derive(Debug)]
pub struct Session {
    stdin: Arc<RwLock<StdioFd>>,
    stdout: Arc<RwLock<StdioFd>>,
    stderr: Arc<RwLock<Option<StdioFd>>>,
    child: Arc<RwLock<Child>>,
    handles: Arc<RwLock<Vec<JoinHandle<Result<(), P9cpuServerError>>>>>,
}

#[derive(Debug)]
pub struct PendingSession {
    ninep: Arc<RwLock<Option<(u16, JoinHandle<Result<(), P9cpuServerError>>)>>>,
}

#[derive(Error, Debug)]
pub enum P9cpuServerError {
    #[error("Failed to spawn: {0}")]
    SpawnFail(#[source] std::io::Error),
    #[error("Session does not exist")]
    SessionNotExist,
    #[error("IO Error: {0}")]
    IoErr(#[source] std::io::Error),
    #[error("Duplicate session id")]
    DuplicateId,
    #[error("Command exited without return code.")]
    NoReturnCode,
    #[error("Command error: {0}")]
    CommandError(#[source] std::io::Error),
    #[error("Cannot open pty device")]
    OpenPtyFail(#[from] nix::Error),
    #[error("Cannot clone file descriptor: {0}")]
    FdCloneFail(#[source] std::io::Error),
    #[error("Cannot create directory: {0}")]
    MkDir(#[source] std::io::Error),
    #[error("Invalid FsTab: {0}")]
    InvalidFsTab(String),
    #[error("Cannot bind listener: {0}")]
    BindFail(#[source] std::io::Error),
    #[error("9p forward not setup")]
    No9pPort,
    #[error("String contains null: {0:?}")]
    StringContainsNull(#[from] std::ffi::NulError),
    #[error("Channel closed")]
    ChannelClosed,
}

impl<T> From<mpsc::error::SendError<T>> for P9cpuServerError {
    fn from(_: mpsc::error::SendError<T>) -> Self {
        Self::ChannelClosed
    }
}

fn parse_fstab_opt(opt: &str) -> (nix::mount::MsFlags, String) {
    let mut opts = vec![];
    let mut flag = nix::mount::MsFlags::empty();
    for f in opt.split(',') {
        if f == "defaults" {
            continue;
        }
        if f == "bind" {
            flag |= nix::mount::MsFlags::MS_BIND;
        } else {
            opts.push(f);
        }
    }
    (flag, opts.join(","))
}

struct MountParams {
    source: Option<CString>,
    target: CString,
    fstype: Option<CString>,
    flags: nix::mount::MsFlags,
    data: Option<CString>,
}

impl TryFrom<FsTab> for MountParams {
    type Error = P9cpuServerError;
    fn try_from(tab: FsTab) -> Result<Self, P9cpuServerError> {
        let FsTab {
            spec: source,
            file: target,
            vfstype: fstype,
            mntops: opt,
            freq: _,
            passno: _,
        } = tab;
        let (flags, data) = parse_fstab_opt(&opt);
        let mnt = MountParams {
            source: Some(CString::new(source)?),
            target: CString::new(target)?,
            fstype: Some(CString::new(fstype)?),
            flags,
            data: Some(CString::new(data)?),
        };
        Ok(mnt)
    }
}

mod pre_exec;

#[derive(Debug, Default)]
pub struct P9cpuServerInner<I> {
    sessions: Arc<RwLock<HashMap<I, Session>>>,
    pending: Arc<RwLock<HashMap<I, PendingSession>>>,
}

impl<SID> P9cpuServerInner<SID>
where
    SID: Eq + Hash + Debug + Display + Sync + Send + Clone + 'static,
{
    async fn get_session<O, R>(&self, sid: &SID, op: O) -> Result<R, P9cpuServerError>
    where
        O: Fn(&Session) -> R,
    {
        let sessions = self.sessions.read().await;
        let info = sessions.get(sid).ok_or(P9cpuServerError::SessionNotExist)?;
        Ok(op(info))
    }

    fn make_cmd(
        &self,
        command: CommandReq,
        ninep_port: Option<u16>,
    ) -> Result<(Command, ChildStdio), P9cpuServerError> {
        let mut cmd = Command::new(command.program);
        cmd.args(command.args);
        let mut user = "nouser".to_string();
        let mut pwd = None;
        for env in command.envs {
            cmd.env(OsStr::from_bytes(&env.key), OsStr::from_bytes(&env.val));
            match env.key.as_slice() {
                b"USER" => {
                    if let Ok(s) = String::from_utf8(env.val) {
                        user = s;
                    }
                }
                b"PWD" => {
                    if let Ok(s) = CString::new(env.val) {
                        pwd = Some(s);
                    }
                }
                _ => {}
            }
        }

        unsafe {
            cmd.pre_exec(move || {
                close_fds::set_fds_cloexec(3, &[]);
                Ok(())
            });
        }
        pre_exec::create_private_root(&mut cmd);
        if command.ninep {
            let Some(ninep_port) = ninep_port else {
                    return Err(P9cpuServerError::No9pPort);
            };
            pre_exec::create_namespace_9p(&mut cmd, command.tmp_mnt, ninep_port, &user)?;
        }
        // if let Some(tmp_mnt) = command.tmp_mnt {
        //     if !command.namespace.is_empty() {
        //         let Some(ninep_port) = ninep_port else {
        //             return Err(P9cpuServerError::No9pPort);
        //         };
        //         pre_exec::create_namespace_9p(
        //             &mut cmd,
        //             command.namespace,
        //             tmp_mnt,
        //             ninep_port,
        //             &user,
        //         )?;
        //     }
        // }
        pre_exec::mount_fstab(&mut cmd, command.fstab)?;
        if let Some(pwd) = pwd {
            unsafe {
                cmd.pre_exec(move || {
                    nix::unistd::chdir(pwd.as_c_str())?;
                    Ok(())
                });
            }
        }
        let child_stdio = if command.tty {
            let result = nix::pty::openpty(None, None).map_err(P9cpuServerError::OpenPtyFail)?;
            let stdin = unsafe { OwnedFd::from_raw_fd(result.slave) };
            let stdout = stdin.try_clone().map_err(P9cpuServerError::FdCloneFail)?;
            let stderr = stdin.try_clone().map_err(P9cpuServerError::FdCloneFail)?;
            cmd.stdin(stdin).stdout(stdout).stderr(stderr);
            unsafe {
                cmd.pre_exec(|| {
                    nix::unistd::setsid()?;
                    nix::ioctl_none_bad!(tiocsctty, libc::TIOCSCTTY);
                    tiocsctty(0)?;
                    Ok(())
                });
            }
            ChildStdio::Pty(unsafe { OwnedFd::from_raw_fd(result.master) })
        } else {
            let (stdin_rd, stdin_wr) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)?;
            let (stdout_rd, stdout_wr) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)?;
            let (stderr_rd, stderr_wr) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)?;
            let stdin = unsafe { OwnedFd::from_raw_fd(stdin_rd) };
            let stdout = unsafe { OwnedFd::from_raw_fd(stdout_wr) };
            let stderr = unsafe { OwnedFd::from_raw_fd(stderr_wr) };
            // use std::io::prelude::*;
            // let (mut reader, mut writer) = os_pipe::pipe().unwrap();

            cmd.stdin(stdin).stdout(stdout).stderr(stderr);
            unsafe {
                ChildStdio::Piped(
                    OwnedFd::from_raw_fd(stdin_wr),
                    OwnedFd::from_raw_fd(stdout_rd),
                    OwnedFd::from_raw_fd(stderr_rd),
                )
            }
        };
        Ok((cmd, child_stdio))
    }

    pub async fn dial(&self, sid: SID) -> Result<(), P9cpuServerError> {
        let mut pending = self.pending.write().await;
        if pending.contains_key(&sid) {
            return Err(P9cpuServerError::DuplicateId);
        }
        let session = PendingSession {
            ninep: Arc::new(RwLock::new(None)),
        };
        log::info!("Session {} created", &sid);
        pending.insert(sid, session);
        Ok(())
    }

    pub async fn start(&self, command: CommandReq, sid: SID) -> Result<(), P9cpuServerError> {
        let Some(PendingSession { ninep }) = self.pending.write().await.remove(&sid) else {
            return Err(P9cpuServerError::SessionNotExist);
        };
        let mut sessions = self.sessions.write().await;
        if sessions.contains_key(&sid) {
            return Err(P9cpuServerError::DuplicateId);
        }
        let mut handles = vec![];
        let mut ninep_port = None;
        if let Some((port, handle)) = ninep.write().await.take() {
            ninep_port = Some(port);
            handles.push(handle);
        }
        let (mut cmd, child_stdio) = self.make_cmd(command, ninep_port)?;
        println!("make command is done, will spawn");

        // let mut child = if ninep_port.is_some() {
        //     let (tx, rx) = oneshot::channel();
        //     let f = async move {
        //         let child = cmd.spawn().map_err(P9cpuServerError::SpawnFail);
        //         tx.send(child).unwrap();
        //     };
        //     std::thread::spawn(move || {
        //         let runtime = tokio::runtime::Builder::new_current_thread()
        //             .enable_all()
        //             .build()
        //             .unwrap();
        //         runtime.block_on(f);
        //     });
        //     rx.await.unwrap()
        // } else {
        //     cmd.spawn().map_err(P9cpuServerError::SpawnFail)
        // }?;
        let (stdin, stdout, stderr) = match child_stdio {
            ChildStdio::Pty(master) => {
                let master_copy = master.try_clone().map_err(P9cpuServerError::FdCloneFail)?;
                (
                    StdioFd::try_from(master).map_err(P9cpuServerError::IoErr)?,
                    StdioFd::try_from(master_copy).map_err(P9cpuServerError::IoErr)?,
                    None,
                )
            }
            ChildStdio::Piped(stdin_wr, stdout_rd, stderr_rd) => {
                log::info!("tty is not used");
                (
                    StdioFd::try_from(stdin_wr).map_err(P9cpuServerError::IoErr)?,
                    StdioFd::try_from(stdout_rd).map_err(P9cpuServerError::IoErr)?,
                    Some(StdioFd::try_from(stderr_rd).map_err(P9cpuServerError::IoErr)?),
                )
            }
        };
        println!("spawn command is done");
        let child = cmd.spawn().map_err(P9cpuServerError::SpawnFail)?;
        let info = Session {
            stdin: Arc::new(RwLock::new(stdin)),
            stdout: Arc::new(RwLock::new(stdout)),
            stderr: Arc::new(RwLock::new(stderr)),
            child: Arc::new(RwLock::new(child)),
            handles: Arc::new(RwLock::new(handles)),
        };
        log::info!("Session {} started", &sid);
        sessions.insert(sid, info);
        Ok(())
    }

    pub async fn stdin(
        &self,
        sid: &SID,
        mut in_stream: impl Stream<Item = Vec<u8>> + Unpin,
    ) -> Result<(), P9cpuServerError> {
        let cmd_stdin = self.get_session(sid, |s| s.stdin.clone()).await?;
        let mut cmd_stdin = cmd_stdin.write().await;
        log::debug!("Session {} stdin stream started", sid);
        while let Some(item) = in_stream.next().await {
            cmd_stdin
                .write_all(&item)
                .await
                .map_err(P9cpuServerError::IoErr)?;
        }
        Ok(())
    }

    // pub async fn stdout<Item>(&self, sid: &SID) -> Result<Receiver<Item>, P9cpuServerError>
    // where
    //     Item: FromVecu8 + Debug + Send + Sync + 'static,
    // {
    //     let cmd_stdout = self.get_session(sid, |s| s.stdout.clone()).await?;
    //     let (tx, rx) = mpsc::channel(10);
    //     let out_handle = tokio::spawn(async move {
    //         let mut out = cmd_stdout.write().await;
    //         if let Err(_e) = send_buf(&mut *out, tx).await {}
    //     });
    //     let handles = self.get_session(sid, |s| s.handles.clone()).await?;
    //     handles.write().await.push(out_handle);
    //     Ok(rx)
    // }

    pub async fn stdout_st(
        &self,
        sid: &SID,
    ) -> Result<impl Stream<Item = Vec<u8>>, P9cpuServerError> {
        let cmd_stdout = self.get_session(sid, |s| s.stdout.clone()).await?;
        let (tx, rx) = mpsc::channel(10);
        let sid_copy = sid.clone();
        let out_handle = tokio::spawn(async move {
            let mut out = cmd_stdout.write().await;
            log::debug!("Session {} stdout stream started", &sid_copy);
            Self::copy_to(&mut *out, tx).await
        });
        let handles = self.get_session(sid, |s| s.handles.clone()).await?;
        handles.write().await.push(out_handle);
        let stream = ReceiverStream::new(rx);
        Ok(stream)
    }

    async fn copy_to(
        src: &mut (impl AsyncRead + Unpin),
        tx: mpsc::Sender<Vec<u8>>,
    ) -> Result<(), P9cpuServerError> {
        loop {
            let mut buf = vec![0; 128];
            let len = src.read(&mut buf).await.map_err(P9cpuServerError::IoErr)?;
            if len == 0 {
                break;
            }
            buf.truncate(len);
            tx.send(buf).await?;
        }
        Ok(())
    }

    pub async fn stderr_st(
        &self,
        sid: &SID,
    ) -> Result<impl Stream<Item = Vec<u8>>, P9cpuServerError> {
        let cmd_stderr = self.get_session(sid, |s| s.stderr.clone()).await?;
        let (tx, rx) = mpsc::channel(10);
        let sid_copy = sid.clone();
        let err_handle = tokio::spawn(async move {
            let mut err = cmd_stderr.write().await;
            let Some(ref mut err) = &mut *err else {
                log::info!("Session {} has no stderr", &sid_copy);
                return Ok(());
            };
            log::debug!("Session {} stderr stream started", &sid_copy);
            Self::copy_to(&mut *err, tx).await
        });
        let handles = self.get_session(sid, |s| s.handles.clone()).await?;
        handles.write().await.push(err_handle);
        let stream = ReceiverStream::new(rx);
        Ok(stream)
    }

    // pub async fn stderr<Item>(&self, sid: &SID) -> Result<Receiver<Item>, P9cpuServerError>
    // where
    //     Item: FromVecu8 + Debug + Send + Sync + 'static,
    // {
    //     let cmd_stderr = self.get_session(sid, |s| s.stderr.clone()).await?;
    //     let (tx, rx) = mpsc::channel(10);
    //     let err_handle = tokio::spawn(async move {
    //         let mut err = cmd_stderr.write().await;
    //         let Some(ref mut err) = &mut *err else {
    //             return;
    //         };
    //         if let Err(_e) = send_buf(&mut *err, tx).await {}
    //     });
    //     let handles = self.get_session(sid, |s| s.handles.clone()).await?;
    //     handles.write().await.push(err_handle);
    //     Ok(rx)
    // }

    // pub async fn ninep_f(
    //     &self,
    //     sid: &SID,
    //     mut in_stream: impl Stream<Item = Vec<u8>> + Unpin,
    // ) -> Result<impl Stream<Item = Vec<u8>>, P9cpuServerError> {

    // }

    // pub async fn ninep_forward<'a, IS, IItem, OItem>(
    //     &self,
    //     sid: &SID,
    //     mut in_stream: IS,
    // ) -> Result<Receiver<OItem>, P9cpuServerError>
    // where
    //     OItem: FromVecu8 + Debug + Send + Sync + 'static,
    //     IS: Stream<Item = IItem> + Unpin + Send + 'static,
    //     IItem: AsBytes<'a> + Send + Sync,
    pub async fn ninep_forward(
        &self,
        sid: &SID,
        mut in_stream: impl Stream<Item = Vec<u8>> + Unpin + Send + 'static,
    ) -> Result<impl Stream<Item = Vec<u8>>, P9cpuServerError> {
        let pending = self.pending.write().await;
        let session = pending.get(sid).ok_or(P9cpuServerError::SessionNotExist)?;
        let addr = std::net::SocketAddrV4::new(std::net::Ipv4Addr::new(127, 0, 0, 1), 0);
        let listener = std::net::TcpListener::bind(addr).map_err(P9cpuServerError::BindFail)?;
        let port = match listener.local_addr() {
            Ok(addr) => Ok(addr.port()),
            Err(e) => Err(P9cpuServerError::BindFail(e)),
        }?;
        log::info!("Session {}: started 9p listener on 127.0.0.1:{}", sid, port);
        let (tcp_listener_tx, mut tcp_listener_rx) = mpsc::channel(1);
        std::thread::spawn(move || {
            async fn accept_timeout(
                std_listener: std::net::TcpListener,
                duration: std::time::Duration,
            ) -> Result<(std::net::TcpStream, std::net::SocketAddr), std::io::Error> {
                std_listener.set_nonblocking(true)?;
                let listener = tokio::net::TcpListener::from_std(std_listener)?;
                let sleep = tokio::time::sleep(duration);
                let maybe_peer = tokio::select! {
                    () = sleep => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "")),
                    maybe_peer = listener.accept() => maybe_peer,
                };
                maybe_peer.and_then(|(stream, peer)| {
                    let std_stream = stream.into_std()?;
                    Ok((std_stream, peer))
                })
            }
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let ret =
                runtime.block_on(accept_timeout(listener, std::time::Duration::from_secs(60)));
            if tcp_listener_tx.blocking_send(ret).is_err() {
                log::error!("tcp_listener_rx is main runtime is down");
            }
        });

        let (tx, rx) = mpsc::channel(10);
        let sid_copy = sid.clone();
        // std::os::linux::net::TcpStreamExt
        let f = async move {
            println!("forwarding thread started");
            let Some(maybe_peer) = tcp_listener_rx.recv().await else {
                return Err(P9cpuServerError::ChannelClosed);
            };
            let Ok((std_stream, peer)) = maybe_peer else {
                println!("get stream fail");
                return Ok::<(), P9cpuServerError>(());
            };
            log::info!(
                "Session {}: get 9p request from client {:?}",
                &sid_copy,
                peer
            );
            std_stream
                .set_nonblocking(true)
                .map_err(P9cpuServerError::IoErr)?;
            let mut stream =
                tokio::net::TcpStream::from_std(std_stream).map_err(P9cpuServerError::IoErr)?;
            let (mut reader, mut writer) = stream.split();
            loop {
                let mut buf = vec![0; 128];
                tokio::select! {
                    len = reader.read(&mut buf) => {
                        let len = len.map_err(P9cpuServerError::IoErr)?;
                        if len == 0 {
                            log::info!("buf from reader is 0");
                            break;
                        }
                        buf.truncate(len);
                        tx.send(buf).await?;
                    }
                    in_item = in_stream.next() => {
                        let Some(in_item) = in_item else {
                            log::info!("in item is none");
                            break
                        };
                        let len = writer.write(&in_item).await.map_err(P9cpuServerError::IoErr)?;
                        if len == 0 {
                            log::info!("send bytes to os client is o");
                            break;
                        }
                    }
                }
            }
            log::info!("Session {}: 9p is done.", &sid_copy);
            Ok(())
        };
        // let (result_tx, mut result_rx) = mpsc::channel(1);
        // std::thread::spawn(move || {
        //     let runtime = tokio::runtime::Builder::new_current_thread()
        //         .enable_all()
        //         .build()
        //         .unwrap();
        //     let ret = runtime.block_on(f);
        //     if result_tx.blocking_send(ret).is_err() {
        //         log::error!("RX in main thread is down");
        //     }
        // });
        // let handle = tokio::spawn(async move {
        //     match result_rx.recv().await {
        //         Some(ret) => ret,
        //         None => Err(P9cpuServerError::ChannelClosed),
        //     }
        // });
        let handle = tokio::spawn(f);
        *session.ninep.write().await = Some((port, handle));
        Ok(ReceiverStream::new(rx))
    }

    pub async fn wait(&self, sid: &SID) -> Result<i32, P9cpuServerError> {
        let child = self.get_session(sid, |s| s.child.clone()).await?;
        let ret = match child.write().await.wait().await {
            Ok(status) => status.code().ok_or(P9cpuServerError::NoReturnCode),
            Err(e) => Err(P9cpuServerError::CommandError(e)),
        };
        println!("child is done");
        let handles = self.get_session(sid, |s| s.handles.clone()).await?;
        for handle in handles.write().await.iter_mut() {
            if let Err(e) = handle.await {
                eprintln!("handle join error {:?}", e);
            }
        }
        self.sessions.write().await.remove(sid);
        log::info!("Session {} is done", &sid);
        ret
    }
}

// async fn send_buf<R, Item>(src: &mut R, tx: mpsc::Sender<Item>) -> Result<()>
// where
//     R: AsyncRead + Unpin,
//     Item: Sync + Send + Debug + 'static + FromVecu8,
// {
//     loop {
//         let mut buf = vec![0; 128];
//         let len = src.read(&mut buf).await?;
//         if len == 0 {
//             break;
//         }
//         buf.truncate(len);
//         tx.send(Item::from_vec_u8(buf)).await?;
//     }
//     Ok(())
// }

pub fn rpc_based() -> rpc::rpc_server::RpcServer {
    rpc::rpc_server::RpcServer {}
}

// #[cfg(test)]
// mod test {
//     #[test]
//     fn test_non_blocking() {
//         let r = nix::pty::openpty(None, None).unwrap();
//         let flag = nix::fcntl::fcntl(r.master, nix::fcntl::FcntlArg::F_GETFL).unwrap();
//         let flag = unsafe { nix::fcntl::OFlag::from_bits_unchecked(flag) };
//         println!("flag = {:?}", flag);

//         let arg = nix::fcntl::FcntlArg::F_SETFL(flag & nix::fcntl::OFlag::O_NONBLOCK);
//         nix::fcntl::fcntl(r.master, arg).unwrap();
//     }
// }
