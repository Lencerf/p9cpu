// use std::{fmt::format, io:};

use std::{collections::HashMap, io::Write};

use anyhow::{Ok, Result};
use async_trait::async_trait;
use futures::StreamExt;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use tokio::{net::UnixListener, sync::mpsc, task::JoinHandle};
use tokio_stream::wrappers::{ReceiverStream, UnixListenerStream};
use tonic::transport::{Channel, Endpoint, Server};

use crate::client::P9cpuCommand;

#[derive(Error, Debug)]
pub enum P9cpuClientError {
    #[error("Command not started")]
    NotStarted,
    #[error("Command exits with {0}")]
    NonZeroExitCode(i32),
    #[error("Command already started")]
    AlreadyStarted,
}

struct SessionInfo {
    sid: uuid::Uuid,
    handles: Vec<JoinHandle<()>>,
    stop_tx: oneshot::Sender<()>,
}

pub struct P9cpuClient {
    stdin_tx: mpsc::Sender<mpsc::Receiver<P9cpuStdinRequest>>,
    rpc_client: p9cpu_client::P9cpuClient<Channel>,
    session_info: Option<SessionInfo>,
}

impl P9cpuClient {
    pub async fn new(_net: &str, addr: &str) -> Result<Self> {
        let channel = Endpoint::from_shared(addr.to_string())?.connect().await?;
        let (stdin_tx, mut stdin_rx) = mpsc::channel(4);
        let client = p9cpu_client::P9cpuClient::new(channel.clone());
        tokio::spawn(async move {
            let mut stdin_client = p9cpu_client::P9cpuClient::new(channel);
            while let Some(receiver) = stdin_rx.recv().await {
                // stdin_client
                let in_stream = ReceiverStream::new(receiver);
                if let Err(e) = stdin_client.stdin(in_stream).await {
                    println!("stdin_client.stdin error = {:?}", e);
                }
            }
        });
        Ok(Self {
            stdin_tx,
            rpc_client: client,
            session_info: None,
        })
    }

    pub async fn start(&mut self, command: P9cpuCommand) -> Result<uuid::Uuid> {
        if self.session_info.is_some() {
            return Err(P9cpuClientError::AlreadyStarted)?;
        }
        let start_req = command;
        let id = self.rpc_client.start(start_req).await?.into_inner().id;
        let sid = uuid::Uuid::from_slice(&id)?;
        let session_id = P9cpuSessionId { id: id.clone() };
        let mut out_stream = self.rpc_client.stdout(session_id).await?.into_inner();
        let out_handle = tokio::spawn(async move {
            while let Some(Result::Ok(resp)) = out_stream.next().await {
                // println!("get buff at {:?}", SystemTime::now());
                if tokio::io::stdout().write_all(&resp.data).await.is_err() {
                    break;
                }
                // println!("{}", String::from_utf8_lossy(&resp.data))
                // stdout
            }
            println!("std out thread is done");
        });
        let session_id = P9cpuSessionId { id: id.clone() };
        let mut err_stream = self.rpc_client.stderr(session_id).await?.into_inner();
        let err_handle = tokio::spawn(async move {
            while let Some(Result::Ok(resp)) = err_stream.next().await {
                // println!("get buff at {:?}", SystemTime::now());
                // std::io::stderr().write_all(&resp.data).unwrap();
                if tokio::io::stderr().write_all(&resp.data).await.is_err() {
                    break;
                }
            }
            println!("std err thread is done");
        });
        let (tx, rx) = mpsc::channel(4);
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        self.stdin_tx.send(rx).await?;
        let id_vec = id.clone();
        let stdin_handle = tokio::spawn(async move {
            let mut id = Some(id_vec);
            loop {
                let mut buf = vec![0u8; 8];
                // stop_rx
                let mut stdin = tokio::io::stdin();
                let Result::Ok(len) = tokio::select! {
                    len = stdin.read(&mut buf) => len,
                    _ = &mut stop_rx => break,
                } else {
                    // println!("stdin read returns err");
                    break
                };
                buf.truncate(len);
                let req = P9cpuStdinRequest {
                    id: id.take(),
                    data: buf,
                };
                if let Err(_) = tx.send(req).await {
                    // println!("send request fail {:?}", e);
                    break;
                }
            }
            // println!("std in thread is done");
        });
        // let stdin_stream = ReceiverStream::new(rx);
        // self.rpc_client.stdin(stdin_stream).await?;
        self.session_info = Some(SessionInfo {
            sid,
            handles: vec![out_handle, err_handle, stdin_handle],
            stop_tx,
        });
        // println!("session started at {:?}", SystemTime::now());
        Ok(sid)
    }

    pub async fn wait(&mut self) -> Result<()> {
        let SessionInfo {
            sid,
            handles,
            stop_tx,
        } = self
            .session_info
            .take()
            .ok_or(P9cpuClientError::NotStarted)?;
        let wait_request = P9cpuSessionId {
            id: sid.into_bytes().to_vec(),
        };
        let ret = match self.rpc_client.wait(wait_request).await {
            Result::Ok(resp) => {
                let code = resp.into_inner().code;
                if code == 0 {
                    Ok(())
                } else {
                    Err(P9cpuClientError::NonZeroExitCode(code))?
                }
            }
            Err(e) => Err(e)?,
        };
        if let Err(()) = stop_tx.send(()) {
            eprintln!("stdin thread not working");
        }
        for handle in handles {
            if let Err(e) = handle.await {
                println!("error = {:?}", e);
            }
        }
        ret
    }
}

pub struct P9cpuServer {}

#[async_trait]
impl crate::server::P9cpuServer for P9cpuServer {
    async fn serve(&self, net: &str, addr: &str) -> Result<()> {
        let p9cpu_service = p9cpu_server::P9cpuServer::new(rpc_server::P9cpuService::default());
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

pub mod rpc_client;
pub mod rpc_server;

tonic::include_proto!("p9cpu");
