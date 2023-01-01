use std::{collections::HashMap, sync::Arc, fmt::Debug};

use crate::{client::ClientError, cmd::CommandReq, rpc::rpc_client::TryOrErrInto, Addr};
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use prost::Message;
use russh::{client::Msg, Channel, ChannelId, ChannelMsg};
use thiserror::Error;
use tokio::{
    net::TcpStream,
    sync::{mpsc, oneshot, RwLock},
    task::JoinHandle,
};
use tokio_stream::wrappers::ReceiverStream;

#[derive(Error, Debug)]
pub enum SshError {
    #[error("Wait cmd error: {0}")]
    WaitError(String),
    #[error("russh lib error: {0}")]
    Russh(#[from] russh::Error),
    #[error("Task join error: {0}")]
    JoinError(#[from] tokio::task::JoinError),
    #[error("Channel closed")]
    ChannelClosed,
    #[error("Sender is gone")]
    SenderGone,
    // ChannelClosed(#[source] Box<dyn std::error::Error + Sync + Send + 'static>),
}

impl From<SshError> for ClientError {
    fn from(error: SshError) -> Self {
        ClientError::Inner(error.into())
    }
}

impl<T> From<mpsc::error::SendError<T>> for SshError 
// where T: Debug + Send + Sync + 'static
{
    fn from(_: mpsc::error::SendError<T>) -> Self {
        // Self::ChannelClosed(Box::new(e))
        Self::ChannelClosed
    }
}

struct ChannelInfo {
    stdin_tx: Arc<RwLock<Option<mpsc::Sender<Vec<u8>>>>>,
    stdout_rx: Arc<RwLock<Option<mpsc::Receiver<Vec<u8>>>>>,
    stderr_rx: Arc<RwLock<Option<mpsc::Receiver<Vec<u8>>>>>,
    cmd_tx: Arc<RwLock<Option<mpsc::Sender<CommandReq>>>>,
    code_rx: Arc<RwLock<Option<mpsc::Receiver<Result<i32, SshError>>>>>,
}

impl ChannelInfo {
    fn new(
        stdout_rx: mpsc::Receiver<Vec<u8>>,
        stderr_rx: mpsc::Receiver<Vec<u8>>,
        stdin_tx: mpsc::Sender<Vec<u8>>,
        code_rx: mpsc::Receiver<Result<i32, SshError>>,
        cmd_tx: mpsc::Sender<CommandReq>,
        // handle: JoinHandle<Result<i32, SshError>>,
    ) -> ChannelInfo {
        ChannelInfo {
            stdout_rx: Arc::new(RwLock::new(Some(stdout_rx))),
            stderr_rx: Arc::new(RwLock::new(Some(stderr_rx))),
            stdin_tx: Arc::new(RwLock::new(Some(stdin_tx))),
            code_rx: Arc::new(RwLock::new(Some(code_rx))),
            cmd_tx: Arc::new(RwLock::new(Some(cmd_tx))),
        }
    }
}

pub struct SshClient {
    handle: Arc<RwLock<russh::client::Handle<Handler>>>,
    channels: Arc<RwLock<HashMap<ChannelId, ChannelInfo>>>,
}

impl SshClient {
    pub async fn new(addr: Addr) -> anyhow::Result<Self> {
        let config = russh::client::Config::default();
        let config = Arc::new(config);
        let handler = Handler {};

        let key = russh_keys::key::KeyPair::generate_ed25519().unwrap();
        let mut agent = russh_keys::agent::client::AgentClient::connect_env().await?;
        agent.add_identity(&key, &[]).await?;

        let mut handle = match addr {
            Addr::Tcp(addr) => russh::client::connect(config, addr, handler).await?,
            Addr::Uds(addr) => {
                unimplemented!();
            }
            Addr::Vsock(addr) => {
                let stream = tokio_vsock::VsockStream::connect(addr.cid(), addr.port()).await?;
                russh::client::connect_stream(config, stream, handler).await?
            }
        };
        handle
            .authenticate_future(
                std::env::var("USER").unwrap_or("user".to_owned()),
                key.clone_public_key()?,
                agent,
            )
            .await
            .1?;
        Ok(Self {
            handle: Arc::new(RwLock::new(handle)),
            channels: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    async fn loop_fn(
        channel: &mut Channel<Msg>,
        out_tx: &mpsc::Sender<Vec<u8>>,
        err_tx: &mpsc::Sender<Vec<u8>>,
        in_rx: &mut mpsc::Receiver<Vec<u8>>,
        cmd_rx: &mut mpsc::Receiver<CommandReq>,
        code_tx: &mpsc::Sender<Result<i32, SshError>>,
    ) -> Result<bool, SshError> {
        tokio::select! {
            Some(msg) = channel.wait() => {
                match msg {
                    ChannelMsg::Data { data } => {
                        out_tx.send(Vec::from(data.as_ref())).await?;
                        Ok(true)
                    }
                    ChannelMsg::ExtendedData { data, ext: 1 } => {
                        err_tx.send(Vec::from(data.as_ref())).await?;
                        Ok(true)
                    }
                    ChannelMsg::ExtendedData { data, ext: 2 } => {
                        code_tx.send(Err(SshError::WaitError(
                            String::from_utf8_lossy(&data).to_string(),
                        ))).await?;
                        Ok(false)
                    }
                    ChannelMsg::ExitStatus { exit_status } => {
                        code_tx.send(Ok(exit_status as i32)).await?;
                        Ok(false)
                    }
                    _ => {
                        println!("get extra unknown message {:?}", msg);
                        Ok(true)
                    }
                }
            }
            Some(item) = in_rx.recv() => {
                channel.data(item.as_slice()).await.unwrap();
                Ok(true)
            }
            Some(req) = cmd_rx.recv() => {
                channel.exec(true, req.encode_to_vec()).await?;
                Ok(true)
            }
            else => {
                Ok(false)
            },
        }
    }
}

#[async_trait]
impl crate::client::ClientInnerT for SshClient {
    type Error = SshError;
    type SessionId = ChannelId;

    async fn dial(&self) -> Result<Self::SessionId, Self::Error> {
        let mut handle = self.handle.write().await;
        let mut channel = handle.channel_open_session().await?;
        // channel.wait()
        let channel_id = channel.id();
        let mut channel_map = self.channels.write().await;
        let (out_tx, out_rx) = mpsc::channel(10);
        let (err_tx, err_rx) = mpsc::channel(10);
        let (in_tx, mut in_rx) = mpsc::channel::<Vec<u8>>(10);
        let (code_tx, code_rx) = mpsc::channel(1);
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<CommandReq>(1);
        tokio::spawn(async move {
            loop {
                let ret = Self::loop_fn(
                    &mut channel,
                    &out_tx,
                    &err_tx,
                    &mut in_rx,
                    &mut cmd_rx,
                    &code_tx,
                )
                .await;
                match ret {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(err) => {
                        if code_tx.send(Err(err)).await.is_err() {
                            log::error!("Cannot send error to code_rx.");
                        }
                        break;
                    }
                }
            }
        });
        let info = ChannelInfo::new(out_rx, err_rx, in_tx, code_rx, cmd_tx);
        channel_map.insert(channel_id, info);
        Ok(channel_id)
    }

    async fn start(&self, sid: Self::SessionId, command: CommandReq) -> Result<(), Self::Error> {
        let channels = self.channels.write().await;
        let info = channels.get(&sid).unwrap();
        let cmd_tx = info.cmd_tx.write().await.take().unwrap();
        cmd_tx.send(command).await.unwrap();
        Ok(())
    }

    type EmptyFuture = TryOrErrInto<JoinHandle<Result<(), Self::Error>>>;

    type ByteVecStream = ReceiverStream<Vec<u8>>;
    async fn stdout(&self, sid: Self::SessionId) -> Result<Self::ByteVecStream, Self::Error> {
        let channels = self.channels.write().await;
        let info = channels.get(&sid).unwrap();
        let stdout_rx = info.stdout_rx.write().await.take().unwrap();
        Ok(ReceiverStream::new(stdout_rx))
    }
    async fn stderr(&self, sid: Self::SessionId) -> Result<Self::ByteVecStream, Self::Error> {
        let channels = self.channels.write().await;
        let info = channels.get(&sid).unwrap();
        let stderr_rx = info.stderr_rx.write().await.take().unwrap();
        Ok(ReceiverStream::new(stderr_rx))
    }

    async fn stdin(
        &self,
        sid: Self::SessionId,
        mut stream: impl Stream<Item = Vec<u8>> + Send + Sync + 'static + Unpin,
    ) -> Self::EmptyFuture {
        let channels = self.channels.write().await;
        let info = channels.get(&sid).unwrap();
        let stdin_tx = info.stdin_tx.write().await.take().unwrap();
        let handle = tokio::spawn(async move {
            while let Some(item) = stream.next().await {
                stdin_tx.send(item).await.unwrap();
            }
            Ok(())
        });
        TryOrErrInto { future: handle }
    }

    async fn ninep_forward(
        &self,
        sid: Self::SessionId,
        stream: impl Stream<Item = Vec<u8>> + Send + Sync + 'static + Unpin,
    ) -> Result<Self::ByteVecStream, Self::Error> {
        unimplemented!()
    }

    async fn wait(&self, sid: Self::SessionId) -> Result<i32, Self::Error> {
        let channels = self.channels.write().await;
        let info = channels.get(&sid).unwrap();
        // info.code_rx.clone()
        let mut code_rx = info.code_rx.write().await.take().unwrap();
        code_rx.recv().await.ok_or(SshError::SenderGone)?
    }
}

// #[derive(Clone)]
struct Handler;

impl Handler {
    fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl russh::client::Handler for Handler {
    type Error = SshError;

    async fn check_server_key(
        self,
        server_public_key: &russh_keys::key::PublicKey,
    ) -> Result<(Self, bool), Self::Error> {
        println!("check_server_key: {:?}", server_public_key);
        Ok((self, true))
    }

    //    async fn check_server_key(self, server_public_key: &key::PublicKey) -> Result<(Self, bool), Self::Error> {
    //        println!("check_server_key: {:?}", server_public_key);
    //        Ok((self, true))
    //    }

    // async fn data(
    //     self,
    //     channel: ChannelId,
    //     data: &[u8],
    //     session: russh::client::Session,
    // ) -> Result<(Self, russh::client::Session), Self::Error> {
    //     println!(
    //         "data on channel {:?}: {:?}",
    //         channel,
    //         std::str::from_utf8(data)
    //     );
    //     // session.
    //     Ok((self, session))
    // }
}
