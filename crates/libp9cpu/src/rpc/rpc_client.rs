use std::pin::Pin;
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures::{Future, Stream, StreamExt};
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_vsock::VsockStream;

use crate::client::P9cpuClientError;
use crate::cmd::CommandReq;
use crate::rpc;
use crate::rpc::{Empty, StartRequest};
use crate::Addr;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint};
use tonic::{Status, Streaming};
use tower::service_fn;

use super::PrependedStream;

pub struct TryOrErrInto<F> {
    future: F,
}

impl<F, R, E1, E2> Future for TryOrErrInto<F>
where
    E1: From<E2>,
    F: Future<Output = Result<Result<R, E1>, E2>> + Unpin,
{
    type Output = Result<R, E1>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.future).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(r)) => Poll::Ready(r),
            Poll::Ready(Err(e)) => Poll::Ready(Err(E1::from(e))),
        }
    }
}

#[derive(Error, Debug)]
pub enum RpcError {
    #[error("RPC error: {0}")]
    Rpc(#[from] Status),
    #[error("Invalid UUID: {0}")]
    InvalidUuid(#[from] uuid::Error),
    #[error("Task join error: {0}")]
    JoinError(#[from] tokio::task::JoinError),
}

impl From<RpcError> for P9cpuClientError {
    fn from(error: RpcError) -> Self {
        P9cpuClientError::Inner(error.into())
    }
}

pub struct RpcClient {
    channel: Channel,
}

impl RpcClient {
    pub async fn new(addr: Addr) -> anyhow::Result<Self> {
        let channel = match addr {
            Addr::Uds(addr) => {
                Endpoint::try_from("http://[::]:50051")?
                    .connect_with_connector(service_fn(move |_| {
                        // Connect to a Uds socket
                        UnixStream::connect(addr.clone())
                    }))
                    .await?
            }
            Addr::Tcp(addr) => {
                Endpoint::from_shared(format!("http://{}:{}", addr.ip(), addr.port()))?
                    .connect()
                    .await?
            }
            Addr::Vsock(addr) => {
                let cid = addr.cid();
                let port = addr.port();
                Endpoint::try_from("http://[::]:50051")?
                    .connect_with_connector(service_fn(move |_| {
                        // Connect to a Uds socket
                        VsockStream::connect(cid, port)
                    }))
                    .await?
            }
        };
        Ok(Self { channel })
    }
}

pub struct ByteStream<I> {
    inner: I,
}

impl<Inner, B> Stream for ByteStream<Inner>
where
    Inner: Stream<Item = Result<B, Status>> + Unpin,
    B: Into<u8>,
{
    type Item = Result<u8, RpcError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.poll_next_unpin(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Ok(b))) => Poll::Ready(Some(Ok(b.into()))),
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e.into()))),
        }
    }
}

pub struct ByteVecStream<I> {
    inner: I,
    session: uuid::Uuid,
    name: &'static str,
}

impl<Inner, B> Stream for ByteVecStream<Inner>
where
    Inner: Stream<Item = Result<B, Status>> + Unpin,
    Vec<u8>: From<B>,
{
    type Item = Vec<u8>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.poll_next_unpin(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Ok(b))) => Poll::Ready(Some(b.into())),
            Poll::Ready(Some(Err(e))) => {
                log::error!("Session {}: {}: {}", self.session, self.name, e);
                Poll::Ready(None)
            }
        }
    }
}

#[async_trait]
impl crate::client::ClientInnerT for RpcClient {
    type Error = RpcError;
    type SessionId = uuid::Uuid;

    async fn dial(&self) -> Result<Self::SessionId, Self::Error> {
        let mut client = crate::rpc::p9cpu_client::P9cpuClient::new(self.channel.clone());
        let id_vec = client.dial(Empty {}).await?.into_inner().id;
        let sid = uuid::Uuid::from_slice(&id_vec)?;
        Ok(sid)
    }

    async fn start(&self, sid: Self::SessionId, command: CommandReq) -> Result<(), Self::Error> {
        let req = StartRequest {
            id: sid.into_bytes().into(),
            cmd: Some(command),
        };
        let mut client = crate::rpc::p9cpu_client::P9cpuClient::new(self.channel.clone());
        client.start(req).await?.into_inner();
        Ok(())
    }

    type EmptyFuture = TryOrErrInto<JoinHandle<Result<(), Self::Error>>>;

    type ByteVecStream = ByteVecStream<Streaming<rpc::P9cpuBytes>>;
    async fn stdout(&self, sid: Self::SessionId) -> Result<Self::ByteVecStream, Self::Error> {
        let request = crate::rpc::P9cpuSessionId {
            id: sid.into_bytes().into(),
        };
        let mut client = crate::rpc::p9cpu_client::P9cpuClient::new(self.channel.clone());
        let out_stream = client.stdout(request).await?.into_inner();
        Ok(ByteVecStream {
            inner: out_stream,
            name: "stdout",
            session: sid,
        })
    }

    async fn stderr(&self, sid: Self::SessionId) -> Result<Self::ByteVecStream, Self::Error> {
        let request = crate::rpc::P9cpuSessionId {
            id: sid.into_bytes().into(),
        };
        let mut client = crate::rpc::p9cpu_client::P9cpuClient::new(self.channel.clone());
        let err_stream = client.stderr(request).await?.into_inner();
        Ok(ByteVecStream {
            inner: err_stream,
            name: "stderr",
            session: sid,
        })
    }

    async fn stdin(
        &self,
        sid: Self::SessionId,
        mut stream: impl Stream<Item = Vec<u8>> + Send + Sync + 'static + Unpin,
    ) -> Self::EmptyFuture {
        let channel = self.channel.clone();
        let handle = tokio::spawn(async move {
            let Some(first_vec) = stream.next().await else {
                return Ok(());
            };
            let first_req = rpc::P9cpuStdinRequest {
                id: Some(sid.into_bytes().into()),
                data: first_vec,
            };
            let req_stream = stream.map(|data| rpc::P9cpuStdinRequest { id: None, data });
            let stream = PrependedStream {
                stream: req_stream,
                item: Some(first_req),
            };
            let mut stdin_client = crate::rpc::p9cpu_client::P9cpuClient::new(channel);
            stdin_client.stdin(stream).await?;
            Ok(())
        });
        TryOrErrInto { future: handle }
    }

    async fn ninep_forward(
        &self,
        sid: Self::SessionId,
        in_stream: impl Stream<Item = Vec<u8>> + Send + Sync + 'static + Unpin,
    ) -> Result<Self::ByteVecStream, Self::Error> {
        let first_req = crate::rpc::NinepForwardRequest {
            id: Some(sid.into_bytes().into()),
            data: vec![],
        };
        let req_stream = in_stream.map(|data| rpc::NinepForwardRequest { data, id: None });
        let stream = PrependedStream {
            stream: req_stream,
            item: Some(first_req),
        };
        let mut client = crate::rpc::p9cpu_client::P9cpuClient::new(self.channel.clone());
        let out_stream = client
            .ninep_forward(stream)
            .await
            .unwrap()
            .into_inner();
        Ok(ByteVecStream {
            inner: out_stream,
            name: "9p out",
            session: sid,
        })
    }

    async fn wait(&self, sid: Self::SessionId) -> Result<i32, Self::Error> {
        let req = crate::rpc::P9cpuSessionId {
            id: sid.into_bytes().to_vec(),
        };
        let mut client = crate::rpc::p9cpu_client::P9cpuClient::new(self.channel.clone());
        let resp = client.wait(req).await?;

        Ok(resp.into_inner().code)
    }
}
