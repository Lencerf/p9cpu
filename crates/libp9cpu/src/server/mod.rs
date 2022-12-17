use std::{
    collections::HashMap, fmt::Debug, hash::Hash, os::unix::prelude::{FromRawFd, OsStrExt}, pin::Pin,
    process::Stdio, sync::Arc, ffi::OsStr,
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

use crate::{fstab::FsTab, rpc, AsBytes, FromVecu8, P9cpuCommand};
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

fn parse_fstab_line(tab: FsTab) -> Result<MountParams, P9cpuServerError> {
    let FsTab {
        spec: source,
        file: target,
        vfstype: fstype,
        mntops: opt,
        freq: _,
        passno: _,
    } = tab;
    let (flags, data) = parse_fstab_opt(&opt);
    Ok(MountParams {
        source: Some(source),
        target,
        fstype: Some(fstype),
        flags,
        // data: if data.len() == 0 { None } else { Some(data) },
        data: Some(data),
    })
}

struct MountParams {
    source: Option<String>,
    target: String,
    fstype: Option<String>,
    flags: nix::mount::MsFlags,
    data: Option<String>,
}

#[derive(Debug, Default)]
pub struct P9cpuServerInner<I> {
    sessions: Arc<RwLock<HashMap<I, Session>>>,
    pending: Arc<RwLock<HashMap<I, PendingSession>>>,
}

fn create_tmp_mnt(cmd: &mut Command, tmp_mnt: String) -> Result<(), P9cpuServerError> {
    std::fs::create_dir_all(&tmp_mnt).map_err(P9cpuServerError::MkDir)?;
    let op = move || {
        // std/src/sys/unix/process/process_unix.rs
        use nix::mount::{mount, MsFlags};
        use nix::sched::{unshare, CloneFlags};
        unshare(CloneFlags::CLONE_NEWNS)?;
        // https://go-review.git.corp.google.com/c/go/+/38471
        mount(
            Option::<&str>::None,
            "/",
            Option::<&str>::None,
            MsFlags::MS_REC | MsFlags::MS_PRIVATE,
            Option::<&str>::None,
        )?;
        mount(
            Some("p9cpu"),
            tmp_mnt.as_str(),
            Some("tmpfs"),
            MsFlags::empty(),
            Option::<&str>::None,
        )?;
        Ok(())
    };
    unsafe {
        cmd.pre_exec(op);
    }
    Ok(())
}

fn cmd_mount_fstab(cmd: &mut Command, fstab: Vec<FsTab>) -> Result<(), P9cpuServerError> {
    let mut mount_params = vec![];
    for tab in fstab {
        mount_params.push(parse_fstab_line(tab)?);
    }
    let mount_fstab = move || {
        for param in &mount_params {
            std::fs::create_dir_all(&param.target)?;
            nix::mount::mount(
                param.source.as_deref(),
                param.target.as_str(),
                param.fstype.as_deref(),
                param.flags,
                param.data.as_deref(),
            )?;
        }
        Ok(())
    };
    unsafe {
        cmd.pre_exec(mount_fstab);
    }
    Ok(())
}

fn cmd_mount_namespace(
    cmd: &mut Command,
    namespace: HashMap<String, String>,
    tmp_mnt: String,
    ninep_port: u16,
) {
    let op = move || {
        use nix::mount::{mount, MsFlags};
        let user = std::env::var("USER");
        let mut user = user.as_deref().unwrap_or("");
        if user.is_empty() {
            user = "nouser";
        }
        let ninep_mount = format!("{}/mnt9p", &tmp_mnt);
        let ninep_opt = format!(
            "version=9p2000.L,trans=tcp,port={},uname={}",
            ninep_port, user
        );
        std::fs::create_dir_all(&ninep_mount)?;
        mount(
            Some("127.0.0.1"),
            ninep_mount.as_str(),
            Some("9p"),
            MsFlags::MS_NODEV | MsFlags::MS_NOSUID,
            Some(ninep_opt).as_deref(),
        )?;
        for (target, mut source) in namespace.iter() {
            std::fs::create_dir_all(target)?;
            if source.is_empty() {
                source = target;
            }
            let local_source = format!("{}{}", &ninep_mount, source);
            mount(
                Some(local_source).as_deref(),
                target.as_str(),
                Option::<&str>::None,
                MsFlags::MS_BIND,
                Option::<&str>::None,
            )?;
        }
        Ok(())
    };
    unsafe {
        cmd.pre_exec(op);
    }
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
        // println!("get p9cpucmmand = {:?}", &command);
        let mut cmd = Command::new(command.program);
        cmd.args(command.args);
        for env in command.env {
            println!("{} {}", String::from_utf8_lossy(&env.key), String::from_utf8_lossy(&env.val));
            cmd.env(OsStr::from_bytes(&env.key), OsStr::from_bytes(&env.val));
        }
        // cmd.env("TERM", "ansi");
        unsafe {
            cmd.pre_exec(|| {
                close_fds::close_open_fds(3, &[]);
                Ok(())
            });
        }
        if let Some(tmp_mnt) = command.tmp_mnt {
            create_tmp_mnt(&mut cmd, tmp_mnt.clone())?;
            if !command.namespace.is_empty() {
                let Some(ninep_port) = ninep_port else {
                    return Err(P9cpuServerError::No9pPort);
                };
                cmd_mount_namespace(&mut cmd, command.namespace, tmp_mnt, ninep_port);
            }
            cmd_mount_fstab(&mut cmd, command.fstab)?;
        }
        // unsafe {
        //     cmd.pre_exec(|| {
        //         nix::unistd::chdir("/bin")?;
        //         // std/src/sys/unix/process/process_unix.rs do_exec()
        //         if let Ok(pwd) = std::env::var("PWD") {
        //             std::env::set_current_dir("/bin")?;
        //         } else {
        //             return Err(std::io::Error::from_raw_os_error(3));
        //         }
        //         Ok(())
        //     });
        // };
        println!("tmp mnt is done");
        let pty_master = if command.tty {
            let result =
                nix::pty::openpty(None, None).map_err(P9cpuServerError::OpenPtyFail)?;
            let stdin = unsafe { std::fs::File::from_raw_fd(result.slave) };
            let stdout = stdin
                .try_clone()
                .map_err(P9cpuServerError::FdCloneFail)?;
            let stderr = stdin
                .try_clone()
                .map_err(P9cpuServerError::FdCloneFail)?;
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
            handles.push(handle);
        }
        let (mut cmd, pty_master) = self.make_cmd(command, ninep_port)?;
        println!("make command is done, will spawn");
        let mut child = cmd.spawn().map_err(P9cpuServerError::SpawnFail)?;
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
        println!("started listener on {:?}", listener.local_addr());
        let port = listener
            .local_addr()
            .map(|addr| addr.port())
            .map_err(|_| P9cpuServerError::BindFail)?;
        let (tx, rx) = mpsc::channel(10);
        let handle = tokio::spawn(async move {
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
        });
        *session.ninep.write().await = Some((port, handle));
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
