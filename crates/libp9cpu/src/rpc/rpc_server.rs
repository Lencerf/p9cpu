use super::p9cpu_server::P9cpu;
use super::{
    Empty, P9cpuBytes, P9cpuSessionId, P9cpuStartRequest, P9cpuStartResponse, P9cpuStdinRequest,
    P9cpuWaitResponse, PrependedStream,
};
use crate::server::P9cpuServerInner;
use anyhow::Result;
use futures::{Stream, TryFutureExt};
use async_trait::async_trait;
use std::fmt::Debug;
use tonic::transport::Server;
use crate::rpc::p9cpu_server;
use std::pin::Pin;

use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status, Streaming};

type RpcResult<T> = Result<Response<T>, Status>;
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
pub struct RpcServer {}

#[async_trait]
impl crate::server::P9cpuServerT for RpcServer {
    async fn serve(&self, net: &str, addr: &str) -> Result<()> {
        let p9cpu_service = p9cpu_server::P9cpuServer::new(P9cpuService::default());
        let router = Server::builder().add_service(p9cpu_service);
        match net {
            "tcp" => router.serve(addr.parse()?).await?,
            "unix" => {
                let uds = UnixListener::bind(addr)?;
                let stream = UnixListenerStream::new(uds);
                router.serve_with_incoming(stream).await?
            }
            _ => {
                unimplemented!()
            }
        }
        Ok(())
    }
}


#[derive(Debug, Default)]
pub struct P9cpuService {
    inner: P9cpuServerInner<uuid::Uuid>,
}

fn vec_to_uuid(v: &Vec<u8>) -> Result<uuid::Uuid, Status> {
    uuid::Uuid::from_slice(v).map_err(|e| Status::invalid_argument(e.to_string()))
}

#[tonic::async_trait]
impl P9cpu for P9cpuService {
    type StdoutStream = Pin<Box<dyn Stream<Item = Result<P9cpuBytes, Status>> + Send>>;
    type StderrStream = Pin<Box<dyn Stream<Item = Result<P9cpuBytes, Status>> + Send>>;

    async fn start(&self, request: Request<P9cpuStartRequest>) -> RpcResult<P9cpuStartResponse> {
        let req = request.into_inner();
        let sid = uuid::Uuid::new_v4();
        self.inner
            .start(req, sid)
            .map_err(|e| Status::internal(e.to_string()))
            .await?;
        let r = P9cpuStartResponse {
            id: sid.into_bytes().into(),
        };
        Ok(Response::new(r))
    }
    async fn stdin(&self, request: Request<Streaming<P9cpuStdinRequest>>) -> RpcResult<Empty> {
        let mut in_stream = request.into_inner();
        let Some(Ok(P9cpuStdinRequest { id: Some(id), data })) = in_stream.next().await else {
            return Err(Status::invalid_argument("no session id."));
        };
        let sid = vec_to_uuid(&id)?;
        let stream = PrependedStream {
            stream: in_stream,
            item: Some(Ok(P9cpuStdinRequest { id: None, data })),
        };
        self.inner
            .stdin(&sid, stream)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(Empty {}))
    }

    async fn stdout(&self, request: Request<P9cpuSessionId>) -> RpcResult<Self::StdoutStream> {
        let sid = vec_to_uuid(&request.into_inner().id)?;
        let rx = self
            .inner
            .stdout(&sid)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let out_stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(out_stream) as Self::StdoutStream))
    }

    async fn stderr(&self, request: Request<P9cpuSessionId>) -> RpcResult<Self::StderrStream> {
        let sid = vec_to_uuid(&request.into_inner().id)?;
        let rx = self
            .inner
            .stderr(&sid)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let err_stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(err_stream) as Self::StderrStream))
    }

    async fn wait(&self, request: Request<P9cpuSessionId>) -> RpcResult<P9cpuWaitResponse> {
        let sid = vec_to_uuid(&request.into_inner().id)?;
        let code = self
            .inner
            .wait(&sid)
            .map_err(|e| Status::internal(e.to_string()))
            .await?;
        Ok(Response::new(P9cpuWaitResponse { code }))
    }
}
