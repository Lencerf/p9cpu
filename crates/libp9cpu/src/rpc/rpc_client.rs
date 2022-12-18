use std::pin::Pin;
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures::{Future, Stream, StreamExt};
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_vsock::VsockStream;

use crate::rpc::{Empty, StartRequest};
use crate::Addr;
use crate::P9cpuCommand;
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
pub enum RpcInnerError {
    #[error("Command not started")]
    NotStarted,
    #[error("Command exits with {0}")]
    NonZeroExitCode(i32),
    #[error("Command already started")]
    AlreadyStarted,
    #[error("RPC error {0}")]
    Rpc(Status),
    #[error("Invalid UUID {0}")]
    InvalidUuid(uuid::Error),
    #[error("Task join error: {0}")]
    JoinError(tokio::task::JoinError),
}

impl From<tokio::task::JoinError> for RpcInnerError {
    fn from(e: tokio::task::JoinError) -> Self {
        RpcInnerError::JoinError(e)
    }
}

pub struct RpcInner {
    channel: Channel,
    rpc_client: crate::rpc::p9cpu_client::P9cpuClient<Channel>,
}

impl RpcInner {
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
        let rpc_client = crate::rpc::p9cpu_client::P9cpuClient::new(channel.clone());
        Ok(Self {
            channel,
            rpc_client,
        })
    }
}

#[async_trait]
impl crate::client::ClientInnerT for RpcInner {
    type Error = RpcInnerError;
    type SessionId = uuid::Uuid;
    async fn dial(&mut self) -> Result<Self::SessionId, Self::Error> {
        let id_vec = self
            .rpc_client
            .dial(Empty {})
            .await
            .map_err(RpcInnerError::Rpc)?
            .into_inner()
            .id;
        let sid = uuid::Uuid::from_slice(&id_vec).map_err(RpcInnerError::InvalidUuid)?;
        Ok(sid)
    }

    async fn start(
        &mut self,
        sid: Self::SessionId,
        command: P9cpuCommand,
    ) -> Result<(), Self::Error> {
        let req = StartRequest {
            id: sid.into_bytes().into(),
            cmd: Some(command),
        };
        self.rpc_client
            .start(req)
            .await
            .map_err(RpcInnerError::Rpc)?
            .into_inner();
        Ok(())
    }

    async fn wait(&mut self, sid: Self::SessionId) -> Result<i32, Self::Error> {
        let req = crate::rpc::P9cpuSessionId {
            id: sid.into_bytes().to_vec(),
        };
        let resp = self.rpc_client.wait(req).await;

        Ok(resp.map_err(RpcInnerError::Rpc)?.into_inner().code)
    }

    type OutStream = Streaming<crate::rpc::P9cpuBytes>;

    async fn stdout(&mut self, sid: Self::SessionId) -> Result<Self::OutStream, Self::Error> {
        let request = crate::rpc::P9cpuSessionId {
            id: sid.into_bytes().into(),
        };
        let out_stream = self.rpc_client.stdout(request).await.unwrap().into_inner();
        Ok(out_stream)
    }

    async fn stderr(&mut self, sid: Self::SessionId) -> Result<Self::OutStream, Self::Error> {
        let request = crate::rpc::P9cpuSessionId {
            id: sid.into_bytes().into(),
        };
        let err_stream = self.rpc_client.stderr(request).await.unwrap().into_inner();
        Ok(err_stream)
    }

    type InStreamItem = crate::rpc::P9cpuStdinRequest;
    type StdinFuture = TryOrErrInto<JoinHandle<Result<(), Self::Error>>>;
    async fn stdin(
        &mut self,
        sid: Self::SessionId,
        mut stream: impl Stream<Item = Self::InStreamItem> + Send + Sync + 'static + Unpin,
    ) -> Self::StdinFuture {
        let channel = self.channel.clone();
        let handle = tokio::spawn(async move {
            let Some(mut first_req) = stream.next().await else {
                return Ok(());
            };
            first_req.id = Some(sid.into_bytes().into());
            let stream = PrependedStream {
                stream,
                item: Some(first_req),
            };
            let mut stdin_client = crate::rpc::p9cpu_client::P9cpuClient::new(channel);
            stdin_client
                .stdin(stream)
                .await
                .map_err(RpcInnerError::Rpc)?;
            Ok(())
        });
        TryOrErrInto { future: handle }
    }

    type NinepInStreamItem = crate::rpc::NinepForwardRequest;
    type NinepOutStream = Streaming<crate::rpc::P9cpuBytes>;
    async fn ninep_forward(
        &mut self,
        sid: Self::SessionId,
        in_stream: impl Stream<Item = Self::NinepInStreamItem> + Send + Sync + 'static + Unpin,
    ) -> Result<Self::NinepOutStream, Self::Error> {
        let first_req = crate::rpc::NinepForwardRequest {
            id: Some(sid.into_bytes().into()),
            data: vec![],
        };
        let stream = PrependedStream {
            stream: in_stream,
            item: Some(first_req),
        };
        let out_stream = self
            .rpc_client
            .ninep_forward(stream)
            .await
            .unwrap()
            .into_inner();
        Ok(out_stream)
    }
}
