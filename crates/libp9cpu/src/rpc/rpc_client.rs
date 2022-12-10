use async_trait::async_trait;
use futures::{Stream, StreamExt};
use thiserror::Error;
use tokio_vsock::VsockStream;

use crate::client::P9cpuCommand;
use crate::Addr;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint};
use tonic::{Status, Streaming};
use tower::service_fn;

use super::PrependedStream;

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
            Addr::Tcp(addr) => Endpoint::from_shared(addr.to_string())?.connect().await?,
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
    async fn start(&mut self, command: P9cpuCommand) -> Result<Self::SessionId, Self::Error> {
        let id_vec = self
            .rpc_client
            .start(command)
            .await
            .map_err(RpcInnerError::Rpc)?
            .into_inner()
            .id;
        let sid = uuid::Uuid::from_slice(&id_vec).map_err(RpcInnerError::InvalidUuid)?;
        Ok(sid)
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
    async fn stdin(
        &mut self,
        sid: Self::SessionId,
        mut stream: impl Stream<Item = Self::InStreamItem> + Send + Sync + 'static + Unpin,
    ) -> Result<(), Self::Error> {
        let Some(mut first_req) = stream.next().await else {
            return Ok(());
        };
        first_req.id = Some(sid.into_bytes().into());
        let stream = PrependedStream {
            stream,
            item: Some(first_req),
        };
        self.rpc_client
            .stdin(stream)
            .await
            .map_err(RpcInnerError::Rpc)?;
        Ok(())
    }

    fn side_channel(&mut self) -> Self {
        let rpc_client = crate::rpc::p9cpu_client::P9cpuClient::new(self.channel.clone());
        Self {
            channel: self.channel.clone(),
            rpc_client,
        }
    }
}
