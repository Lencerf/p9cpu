use super::p9cpu_server::P9cpu;
use super::{
    Empty, P9cpuBytes, P9cpuSessionId, P9cpuStartRequest, P9cpuStartResponse, P9cpuStdinRequest, P9cpuWaitResponse,
};
use anyhow::Result;
use futures::Stream;

use std::fmt::Debug;
use std::pin::Pin;
use std::process::Stdio;

use std::vec;
use std::{collections::HashMap, sync::Arc};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
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
pub struct Session {
    stdin: Arc<RwLock<ChildStdin>>,
    stdout: Arc<RwLock<ChildStdout>>,
    stderr: Arc<RwLock<ChildStderr>>,
    child: Arc<RwLock<Child>>,
    handles: Arc<RwLock<Vec<JoinHandle<()>>>>,
}

#[derive(Debug, Default)]
pub struct P9cpuService {
    sessions: Arc<RwLock<HashMap<uuid::Uuid, Session>>>,
}

impl P9cpuService {
    async fn add_session(&self, mut child: Child) -> uuid::Uuid {
        let id = uuid::Uuid::new_v4();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
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
        let ret = Command::new(req.program)
            .args(req.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let id = match ret {
            Ok(child) => self.add_session(child).await,
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
        let cmd_stdin = self.get_session(&sid, |s| s.stdin.clone()).await?;
        let mut cmd_stdin = cmd_stdin.write().await;
        cmd_stdin.write_all(&data).await?;
        while let Some(Ok(req)) = in_stream.next().await {
            cmd_stdin.write_all(&req.data).await?;
        }
        Ok(Response::new(Empty {}))
    }

    async fn stderr(&self, request: Request<P9cpuSessionId>) -> RpcResult<Self::StderrStream> {
        let sid = vec_to_uuid(&request.into_inner().id)?;
        println!("stderr called for session {:?}", &sid);
        let cmd_stderr = self.get_session(&sid, |s| s.stderr.clone()).await?;
        let (tx, rx) = mpsc::channel(4);
        let stderr_handle = tokio::spawn(async move {
            let mut err = cmd_stderr.write().await;
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
        let (tx, rx) = mpsc::channel(4);
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
        tx.send(Ok(op(buf))).await?;
        // println!("send buf to channel at {:?}", SystemTime::now());
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
