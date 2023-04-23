use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use russh::server::{Auth, Msg, Session};
use russh::{server, ChannelMsg};
// use russh::server::{Server};
use crate::server::{P9cpuServerError, P9cpuServerInner};
use crate::{Addr, cmd};
use anyhow::Result;
use russh::{Channel, ChannelId};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use prost::Message;

#[derive(Debug, Error)]
pub enum SshError {
    #[error("russh lib error: {0}")]
    Russh(#[from] russh::Error),
    #[error("cpud error: {0}")]
    Cpud(#[from] P9cpuServerError),
    #[error("Invalid data: {0}")]
    InvalidProto(#[from] prost::DecodeError),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

pub struct SshServer {}

#[async_trait]
impl crate::server::P9cpuServerT for SshServer {
    type Error = SshError;
    async fn serve(&self, addr: Addr) -> Result<(), SshError> {
        let client_key = russh_keys::key::KeyPair::generate_ed25519().unwrap();
        let client_pubkey = Arc::new(client_key.clone_public_key().unwrap());
        let mut config = russh::server::Config::default();
        // config.connection_timeout = Some(std::time::Duration::from_secs(3));
        config.connection_timeout = None;
        config.auth_rejection_time = std::time::Duration::from_secs(3);
        config
            .keys
            .push(russh_keys::key::KeyPair::generate_ed25519().unwrap());
        let config = Arc::new(config);
        match addr {
            Addr::Tcp(addr) => {
                let listener = TcpListener::bind(addr).await?;
                while let Ok((socket, _)) = listener.accept().await {
                    let config = config.clone();
                    // let server = server.new_client(socket.peer_addr().ok());
                    let inner: P9cpuServerInner<ChannelId> = P9cpuServerInner::new();
                    let handler = ServerHandler {
                        inner: Arc::new(inner),
                    };
                    tokio::spawn(russh::server::run_stream(config, socket, handler));
                }
            }
            Addr::Uds(addr) => {
                unimplemented!()
            }
            Addr::Vsock(addr) => {
                let mut listener = tokio_vsock::VsockListener::bind(addr.cid(), addr.port())?;
                while let Ok((socket, _)) = listener.accept().await {
                    let config = config.clone();
                    // let server = server.new_client(socket.peer_addr().ok());
                    let inner: P9cpuServerInner<ChannelId> = P9cpuServerInner::new();
                    let handler = ServerHandler {
                        inner: Arc::new(inner),
                    };
                    tokio::spawn(russh::server::run_stream(config, socket, handler));
                }
            }
        }
        Ok(())
    }
}

pub struct ServerHandler {
    inner: Arc<P9cpuServerInner<ChannelId>>,
}

#[async_trait]
impl server::Handler for ServerHandler {
    type Error = SshError;

    async fn channel_open_session(
        self,
        channel: Channel<Msg>,
        session: Session,
    ) -> Result<(Self, bool, Session), Self::Error> {
        self.inner.dial(channel.id()).await?;
        Ok((self, true, session))
    }

    async fn data(
        self,
        channel: ChannelId,
        data: &[u8],
        session: Session,
    ) -> Result<(Self, Session), Self::Error> {
        self.inner.stdin(&channel, data).await?;
        Ok((self, session))
    }

    async fn auth_publickey(self, _: &str, _: &russh_keys::key::PublicKey) -> Result<(Self, Auth), Self::Error> {
        Ok((self, server::Auth::Accept))
    }

    async fn exec_request(
        self,
        channel: ChannelId,
        data: &[u8],
        session: Session,
    ) -> Result<(Self, Session), Self::Error> {
        // self.inner.start(command, sid)
        let req:cmd::CommandReq = prost::Message::decode(data)?;
        self.inner.start(req, channel).await?;
        let mut out_stream = self.inner.stdout_st(&channel).await?;
        let mut err_stream = self.inner.stderr_st(&channel).await?;
        let handle = session.handle();
        tokio::spawn(async move {
            while let Some(data) = out_stream.next().await {
                handle.data(channel, data.into()).await.unwrap();
            }
        });
        let handle = session.handle();
        tokio::spawn(async move {
            while let Some(data) = err_stream.next().await {
                handle.extended_data(channel, 1, data.into()).await.unwrap();
            }
        });
        let handle = session.handle();
        let inner = self.inner.clone();
        tokio::spawn(async move {
            match inner.wait(&channel).await {
                Ok(ret) => {
                    handle.exit_status_request(channel, ret as u32).await.unwrap();
                },
                Err(e) => {
                    handle.extended_data(channel, 2, e.to_string().into()).await.unwrap();
                }
            }
            handle.close(channel).await.unwrap();
        });
        Ok((self, session))
    }
}
