use super::p9cpu_server::P9cpu;
use super::{
    Empty, P9cpuBytes, P9cpuSessionId, P9cpuStartRequest, P9cpuStartResponse, P9cpuStdinRequest,
    P9cpuWaitResponse,
};
use anyhow::Result;
use futures::Stream;

use std::fmt::Debug;
use std::os::unix::prelude::FromRawFd;
use std::pin::Pin;
use std::process::Stdio;

use std::rc::Rc;
use std::vec;
use std::{collections::HashMap, sync::Arc};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, RwLock};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use tonic::{Code, Request, Response, Status, Streaming};

type RpcResult<T> = Result<Response<T>, Status>;

fn vec_to_uuid(v: &Vec<u8>) -> Result<uuid::Uuid, Status> {
    uuid::Uuid::from_slice(v).map_err(|e| Status::invalid_argument(e.to_string()))
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

#[derive(Debug, Default)]
pub struct P9cpuService {
    sessions: Arc<RwLock<HashMap<uuid::Uuid, Session>>>,
}

impl P9cpuService {
    async fn add_session(
        &self,
        mut child: Child,
        pty_master: Option<tokio::fs::File>,
    ) -> uuid::Uuid {
        let id = uuid::Uuid::new_v4();
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
        self.sessions.write().await.insert(id, info);
        id
    }

    async fn get_session<O, R>(&self, sid: &uuid::Uuid, op: O) -> Result<R, Status>
    where
        O: Fn(&Session) -> R,
    {
        let sessions = self.sessions.read().await;
        let info = sessions
            .get(sid)
            .ok_or_else(|| Status::not_found(format!("UUID {:?} does not exist.", &sid)))?;
        Ok(op(info))
    }
}

#[tonic::async_trait]
impl P9cpu for P9cpuService {
    // type StdoutStream = Pin<Box<dyn Stream<Item = Result<P9cpuStdoutResponse, Status>> + Send>>;
    type StdoutStream = Pin<Box<dyn Stream<Item = Result<P9cpuBytes, Status>> + Send>>;
    type StderrStream = Pin<Box<dyn Stream<Item = Result<P9cpuBytes, Status>> + Send>>;

    async fn start(&self, request: Request<P9cpuStartRequest>) -> RpcResult<P9cpuStartResponse> {
        let req = request.into_inner();
        let mut cmd = Command::new(req.program);
        cmd.args(req.args);
        let pty_master = if req.tty {
            let ptm_fd =
                unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
            unsafe {
                libc::grantpt(ptm_fd);
                libc::unlockpt(ptm_fd);
            }
            let mut buf: Vec<_> = vec![0; 100];
            unsafe { libc::ptsname_r(ptm_fd, buf.as_mut_ptr() as *mut libc::c_char, 100) };
            let pts_fd = unsafe { libc::open(buf.as_ptr() as *const libc::c_char, libc::O_RDWR) };
            // let result = nix::pty::openpty(None, None)
            //     .map_err(|e| Status::internal(format!("Cannot open tty: {:?}", e)))?;
            cmd.stdin(unsafe { Stdio::from_raw_fd(pts_fd) })
                .stdout(unsafe { Stdio::from_raw_fd(pts_fd) })
                .stderr(unsafe { Stdio::from_raw_fd(pts_fd) });
            unsafe {
                cmd.pre_exec(|| {
                    nix::unistd::setsid().unwrap();
                    libc::ioctl(0, libc::TIOCSCTTY);
                    Result::Ok(())
                });
            }
            Some(unsafe { tokio::fs::File::from_raw_fd(ptm_fd) })
        } else {
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            None
        };
        let id = match cmd.spawn() {
            Ok(child) => self.add_session(child, pty_master).await,
            Err(e) => return Err(Status::new(Code::InvalidArgument, e.to_string())),
        };
        println!("created session {:?}", &id);
        let r = P9cpuStartResponse {
            id: id.as_bytes().to_vec(),
        };
        Ok(Response::new(r))
    }

    async fn stdin(&self, request: Request<Streaming<P9cpuStdinRequest>>) -> RpcResult<Empty> {
        let mut in_stream = request.into_inner();
        let Some(Ok(P9cpuStdinRequest { id: Some(id), data })) = in_stream.next().await else {
            return Err(Status::invalid_argument("no session id."));
        };
        let sid = vec_to_uuid(&id)?;
        // println!("stdin called for session {:?}", &sid);
        let cmd_stdin = self.get_session(&sid, |s| s.stdin.clone()).await?;
        // println!("get arc stdin");
        let mut cmd_stdin = cmd_stdin.write().await;
        // println!("get write to stdin");
        cmd_stdin.write(&data).await?;
        // println!("first {:?} write to child stdin", data);
        while let Some(Ok(req)) = in_stream.next().await {
            cmd_stdin.write(&req.data).await?;
            // println!("{:?} write to child stdin", req.data);
        }
        Ok(Response::new(Empty {}))
    }

    async fn stderr(&self, request: Request<P9cpuSessionId>) -> RpcResult<Self::StderrStream> {
        let sid = vec_to_uuid(&request.into_inner().id)?;
        println!("stderr called for session {:?}", &sid);
        let cmd_stderr = self.get_session(&sid, |s| s.stderr.clone()).await?;
        let (tx, rx) = mpsc::channel(1);
        let stderr_handle = tokio::spawn(async move {
            let mut err = cmd_stderr.write().await;
            let Some(ref mut err) = &mut *err else {
                return;
            };
            if let Err(e) = send_buf(&mut *err, tx, |buf| P9cpuBytes { data: buf }).await {
                println!("err = {:?}", e)
            }
            println!("stderr done");
        });
        let handles = self.get_session(&sid, |s| s.handles.clone()).await?;
        handles.write().await.push(stderr_handle);
        let err_stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(err_stream) as Self::StderrStream))
    }

    async fn stdout(&self, request: Request<P9cpuSessionId>) -> RpcResult<Self::StdoutStream> {
        let sid = vec_to_uuid(&request.into_inner().id)?;
        println!("stdout_st called for session {:?}", &sid);
        let cmd_stdout = self.get_session(&sid, |info| info.stdout.clone()).await?;
        let (tx, rx) = mpsc::channel(1);
        let stdout_handle = tokio::spawn(async move {
            let mut out = cmd_stdout.write().await;
            // out.read(buf)
            if let Err(e) = send_buf(&mut *out, tx, |buf| P9cpuBytes { data: buf }).await {
                println!("Error = {:?}", e)
            }
            println!("done");
        });
        let handles = self.get_session(&sid, |s| s.handles.clone()).await?;
        handles.write().await.push(stdout_handle);
        let out_stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(out_stream) as Self::StdoutStream))
    }

    async fn wait(&self, request: Request<P9cpuSessionId>) -> RpcResult<P9cpuWaitResponse> {
        let sid = vec_to_uuid(&request.into_inner().id)?;
        println!("wait called for session {:?}", &sid);
        let child = self.get_session(&sid, |info| info.child.clone()).await?;

        let ret = match child.write().await.wait().await {
            Result::Ok(status) => {
                if let Some(code) = status.code() {
                    Ok(Response::new(P9cpuWaitResponse { code }))
                } else {
                    Err(Status::internal(
                        "Command is done, but no exit code found.".to_string(),
                    ))
                }
            }
            Err(e) => Err(Status::internal(e.to_string())),
        };

        let handles = self.get_session(&sid, |info| info.handles.clone()).await?;
        for handle in handles.write().await.iter_mut() {
            handle.await.map_err(|e| Status::internal(e.to_string()))?;
        }
        self.sessions.write().await.remove(&sid);
        println!("session {:?} is done", &sid);
        ret
    }
}

async fn send_buf<R, D, O, E>(src: &mut R, tx: mpsc::Sender<Result<D, E>>, op: O) -> Result<()>
where
    R: AsyncRead + Unpin,
    O: Fn(Vec<u8>) -> D,
    D: Sync + Send + Debug + 'static,
    E: Sync + Send + 'static + std::error::Error,
{
    loop {
        let mut buf = vec![0; 1];
        if src.read(&mut buf).await? == 0 {
            break;
        }
        // let s = String::from_utf8(buf.clone());
        tx.send(Ok(op(buf))).await?;
        // println!(
        //     "send buf {} to channel at {:?}",
        //     s.unwrap(),
        //     std::time::SystemTime::now()
        // );
    }
    Ok(())
}

mod test {
    #[test]
    fn testtry() {
        let uid: Result<[u8; 16], String> = vec![1].try_into().map_err(|e| format!("{:?}", e));
        println!("{:?}", uid);
    }
}
