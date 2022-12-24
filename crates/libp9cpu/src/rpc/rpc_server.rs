use super::p9cpu_server::P9cpu;
use super::{
    Empty, NinepForwardRequest, P9cpuBytes, P9cpuSessionId, P9cpuStdinRequest, P9cpuWaitResponse,
    PrependedStream, StartRequest,
};
use crate::rpc;
use crate::rpc::p9cpu_server;
use crate::server::{P9cpuServerError, P9cpuServerInner};
use crate::Addr;
use anyhow::Result;
use async_trait::async_trait;
use futures::{Stream, StreamExt, TryFutureExt};
use std::fmt::Debug;
use std::pin::Pin;
use std::task::Poll;
use tokio_vsock::{VsockListener, VsockStream};
use tonic::transport::Server;

use tokio_stream::wrappers::ReceiverStream;
// use tokio_stream::StreamExt;
use tonic::{Request, Response, Status, Streaming};

type RpcResult<T> = Result<Response<T>, Status>;
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;

struct VsockListenerStream {
    listener: VsockListener,
}

impl Stream for VsockListenerStream {
    type Item = std::io::Result<VsockStream>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match self.listener.poll_accept(cx) {
            Poll::Ready(Ok((stream, _))) => Poll::Ready(Some(Ok(stream))),
            Poll::Ready(Err(err)) => Poll::Ready(Some(Err(err))),
            Poll::Pending => Poll::Pending,
        }
    }
}

pub struct RpcServer {}

#[async_trait]
impl crate::server::P9cpuServerT for RpcServer {
    async fn serve(&self, addr: Addr) -> Result<()> {
        let p9cpu_service = p9cpu_server::P9cpuServer::new(P9cpuService::default());
        let router = Server::builder().add_service(p9cpu_service);
        match addr {
            Addr::Tcp(addr) => router.serve(addr).await?,
            Addr::Uds(addr) => {
                let uds = UnixListener::bind(addr)?;
                let stream = UnixListenerStream::new(uds);
                router.serve_with_incoming(stream).await?
            }
            Addr::Vsock(addr) => {
                let listener = VsockListener::bind(addr.cid(), addr.port())?;
                let stream = VsockListenerStream { listener };
                router.serve_with_incoming(stream).await?
            }
        }
        // match net {
        //     "tcp" => router.serve(addr.parse()?).await?,
        //     "unix" => {
        //         let uds = UnixListener::bind(addr)?;
        //         let stream = UnixListenerStream::new(uds);
        //         router.serve_with_incoming(stream).await?
        //     }
        //     "vsock" => VsockListener::bind(cid, port),
        //     _ => {
        //         unimplemented!()
        //     }
        // }
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

// pub struct ByteStream<I> {
//     inner: I,
// }

// impl<Inner, B> Stream for ByteStream<Inner>
// where
//     Inner: Stream<Item = Result<B, Status>> + Unpin,
//     B: Into<u8>,
// {
//     type Item = Result<u8, RpcInnerError>;
//     fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
//         match self.inner.poll_next_unpin(cx) {
//             Poll::Pending => Poll::Pending,
//             Poll::Ready(None) => Poll::Ready(None),
//             Poll::Ready(Some(Ok(b))) => Poll::Ready(Some(Ok(b.into()))),
//             Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e.into()))),
//         }
//     }
// }
impl From<P9cpuServerError> for tonic::Status {
    fn from(e: P9cpuServerError) -> Self {
        tonic::Status::internal(e.to_string())
    }
}

#[tonic::async_trait]
impl P9cpu for P9cpuService {
    type StdoutStream = Pin<Box<dyn Stream<Item = Result<P9cpuBytes, Status>> + Send>>;
    type StderrStream = Pin<Box<dyn Stream<Item = Result<P9cpuBytes, Status>> + Send>>;
    type NinepForwardStream = Pin<Box<dyn Stream<Item = Result<P9cpuBytes, Status>> + Send>>;

    async fn ttyin(&self, request: Request<Streaming<rpc::TtyinRequest>>) -> RpcResult<Empty> {
        let mut in_stream = request.into_inner();
        let Some(Ok(rpc::TtyinRequest { id: Some(id), byte:first_byte })) = in_stream.next().await else {
            return Err(Status::invalid_argument("no session id."));
        };
        let sid = vec_to_uuid(&id)?;
        let byte_stream = in_stream.scan((), |_state, req| match req {
            Ok(r) => futures::future::ready(Some(r.byte as u8)),
            Err(e) => futures::future::ready(None),
        });
        let stream = PrependedStream {
            item: Some(first_byte as u8),
            stream: byte_stream,
        };
        self.inner.ttyin(&sid, stream).await?;
        Ok(Response::new(Empty {}))
    }

    async fn start(&self, request: Request<StartRequest>) -> RpcResult<Empty> {
        let StartRequest { id, cmd: Some(cmd) } = request.into_inner() else {
            return Err(Status::invalid_argument("No cmd provided."));
        };
        let sid = vec_to_uuid(&id)?;
        // let Some(cmd) = request.
        self.inner
            .start(cmd, sid)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(Empty {}))
    }

    async fn stdin(&self, request: Request<Streaming<P9cpuStdinRequest>>) -> RpcResult<Empty> {
        let mut in_stream = request.into_inner();
        let Some(Ok(P9cpuStdinRequest { id: Some(id), data })) = in_stream.next().await else {
            return Err(Status::invalid_argument("no session id."));
        };
        let sid = vec_to_uuid(&id)?;
        let byte_stream = in_stream.scan((), |_s, req| match req {
            Ok(r) => futures::future::ready(Some(r.data)),
            Err(e) => {
                log::error!("Session {} stdin stream error: {:?}", sid, e);
                futures::future::ready(None)
            }
        });
        let stream = PrependedStream {
            stream: byte_stream,
            item: Some(data),
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

    async fn dial(&self, _: Request<Empty>) -> RpcResult<P9cpuSessionId> {
        let sid = uuid::Uuid::new_v4();
        self.inner
            .dial(sid)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let r = P9cpuSessionId {
            id: sid.into_bytes().into(),
        };
        Ok(Response::new(r))
    }

    async fn ninep_forward(
        &self,
        request: Request<Streaming<NinepForwardRequest>>,
    ) -> RpcResult<Self::NinepForwardStream> {
        let mut in_stream = request.into_inner();
        let Some(Ok(NinepForwardRequest { id: Some(id), data: _ })) = in_stream.next().await else {
            return Err(Status::invalid_argument("no session id."));
        };
        let sid = vec_to_uuid(&id)?;
        let rx = self
            .inner
            .ninep_forward(&sid, in_stream)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let ninep_stream = ReceiverStream::new(rx);
        Ok(Response::new(
            Box::pin(ninep_stream) as Self::NinepForwardStream
        ))
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
