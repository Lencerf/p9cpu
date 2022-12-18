use std::{
    collections::HashMap,
    ffi::{CString, OsStr},
    fmt::Debug,
    hash::Hash,
    os::unix::prelude::{AsRawFd, FromRawFd, OsStrExt, RawFd},
    pin::Pin,
    process::Stdio,
    sync::Arc,
};

use crate::{fstab::FsTab, rpc, AsBytes, FromVecu8, P9cpuCommand};
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
        oneshot, RwLock,
    },
    task::JoinHandle,
};

#[async_trait]
pub trait P9cpuServerT {
    async fn serve(&self, addr: crate::Addr) -> Result<()>;
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

#[derive(Debug)]
pub struct PendingSession {
    ninep: Arc<RwLock<Option<(u16, JoinHandle<()>)>>>,
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
    #[error("Cannot create directory: {0}")]
    MkDir(std::io::Error),
    #[error("Invalid FsTab: {0}")]
    InvalidFsTab(String),
    #[error("Cannot bind listener")]
    BindFail,
    #[error("9p forward not setup")]
    No9pPort,
    #[error("String contains null: {0:?}")]
    StringContainsNull(std::ffi::NulError),
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
        let op = P9cpuServerError::StringContainsNull;
        let mnt = MountParams {
            source: Some(CString::new(source).map_err(op)?),
            target: CString::new(target).map_err(op)?,
            fstype: Some(CString::new(fstype).map_err(op)?),
            flags,
            data: Some(CString::new(data).map_err(op)?),
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

    fn make_cmd(
        &self,
        command: P9cpuCommand,
        ninep_port: Option<u16>,
    ) -> Result<(Command, Option<File>), P9cpuServerError> {
        let mut cmd = Command::new(command.program);
        cmd.args(command.args);
        let mut user = "nouser".to_string();
        let mut pwd = None;
        for env in command.env {
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
        if let Some(tmp_mnt) = command.tmp_mnt {
            if !command.namespace.is_empty() {
                let Some(ninep_port) = ninep_port else {
                    return Err(P9cpuServerError::No9pPort);
                };
                pre_exec::create_namespace_9p(
                    &mut cmd,
                    command.namespace,
                    tmp_mnt,
                    ninep_port,
                    &user,
                )?;
            }
        }
        pre_exec::mount_fstab(&mut cmd, command.fstab)?;
        if let Some(pwd) = pwd {
            unsafe {
                cmd.pre_exec(move || {
                    nix::unistd::chdir(pwd.as_c_str())?;
                    Ok(())
                });
            }
        }
        println!("tmp mnt is done");
        let pty_master = if command.tty {
            let result = nix::pty::openpty(None, None).map_err(P9cpuServerError::OpenPtyFail)?;
            let stdin = unsafe { std::fs::File::from_raw_fd(result.slave) };
            let stdout = stdin.try_clone().map_err(P9cpuServerError::FdCloneFail)?;
            let stderr = stdin.try_clone().map_err(P9cpuServerError::FdCloneFail)?;
            // Stdio::
            cmd.stdin(stdin).stdout(stdout).stderr(stderr);
            unsafe {
                cmd.pre_exec(|| {
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

    pub async fn dial(&self, sid: SID) -> Result<(), P9cpuServerError> {
        let mut pending = self.pending.write().await;
        if pending.contains_key(&sid) {
            return Err(P9cpuServerError::DuplicateId);
        }
        let session = PendingSession {
            ninep: Arc::new(RwLock::new(None)),
        };
        pending.insert(sid, session);
        Ok(())
    }

    pub async fn start(&self, command: P9cpuCommand, sid: SID) -> Result<(), P9cpuServerError> {
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
            // handles.push(handle);
        }
        let (mut cmd, pty_master) = self.make_cmd(command, ninep_port)?;
        println!("make command is done, will spawn");
        let mut child = if ninep_port.is_some() {
            let (tx, rx) = oneshot::channel();
            let f = async move {
                let child = cmd.spawn().map_err(P9cpuServerError::SpawnFail);
                tx.send(child).unwrap();
            };
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(f);
            });
            rx.await.unwrap()
        } else {
            cmd.spawn().map_err(P9cpuServerError::SpawnFail)
        }?;
        println!("spawn command is done");
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
            handles: Arc::new(RwLock::new(handles)),
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

    pub async fn ninep_forward<'a, IS, IItem, OItem>(
        &self,
        sid: &SID,
        mut in_stream: IS,
    ) -> Result<Receiver<OItem>, P9cpuServerError>
    where
        OItem: FromVecu8 + Debug + Send + Sync + 'static,
        IS: Stream<Item = IItem> + Unpin + Send + 'static,
        IItem: AsBytes<'a> + Send + Sync,
    {
        let pending = self.pending.write().await;
        let session = pending.get(sid).ok_or(P9cpuServerError::SessionNotExist)?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|_| P9cpuServerError::BindFail)?;
        let port = listener
            .local_addr()
            .map(|addr| addr.port())
            .map_err(|_| P9cpuServerError::BindFail)?;
        let listener_fd = listener.as_raw_fd();
        println!(
            "started listener on {:?}, fd={}",
            listener.local_addr(),
            listener_fd
        );
        let (tx, rx) = mpsc::channel(10);
        let f = async move {
            println!("forwarding thread started");
            let Ok((mut stream, peer)) = listener.accept().await else {
                println!("get stream fail");
                return;
            };
            println!("get ninep request from client {:?}", peer);
            let (mut reader, mut writer) = stream.split();
            loop {
                let mut buf = vec![0; 128];
                tokio::select! {
                    len = reader.read(&mut buf) => {
                        let Ok(len) = len else {
                            println!("buf from reader is err");
                            break
                        };
                        if len == 0 {
                            println!("buf from reader is 0");
                            break;
                        }
                        let item = OItem::from_vec_u8(buf[0..len].to_vec());
                        if tx.send(item).await.is_err() {
                            println!("tx is gone");
                            break;
                        }
                    }
                    in_item = in_stream.next() => {
                        let Some(in_item) = in_item else {
                            println!("in item is none");
                            break
                        };
                        let Ok(len) = writer.write(in_item.as_bytes()).await else {
                            println!("send bytes to os client fail");
                            break
                        };
                        if len == 0 {
                            println!("send bytes to os client is o");
                            break;
                        }
                    }
                }
            }
        };
        let handle = tokio::spawn(f);
        *session.ninep.write().await = Some((port, handle));
        Ok(rx)
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
