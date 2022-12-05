// use crate::p9cpu::p9cpu_server::{P9cpu, P9cpuServer};
// use crate::p9cpu::{
//     P9cpuStartRequest, P9cpuStartResponse, P9cpuStdInRequest, P9cpuStdInResponse,
//     P9cpuStdOutRequest, P9cpuStdOutResponse,
// };
// use futures::Stream;
// use std::pin::Pin;
// use std::process::Command;
// use tonic::{Request, Response, Status};

// type RpcResult<T> = Result<Response<T>, Status>;

// #[derive(Debug, Default)]
// pub struct P9cpuService {}

// #[tonic::async_trait]
// impl P9cpu for P9cpuService {
//     type StdOutStream = Pin<Box<dyn Stream<Item = Result<P9cpuStdOutResponse, Status>> + Send>>;

//     async fn start(&self, request: Request<P9cpuStartRequest>) -> RpcResult<P9cpuStartResponse> {
//         let req = request.into_inner();
//         let ret = Command::new(req.binary)
//             .args(req.args)
//             .spawn()
//             .and_then(|mut c| c.wait());
//         let r = P9cpuStartResponse {
//             status: 1,
//             uid: ret.err().map_or("".to_string(), |e| e.to_string()),
//         };
//         Ok(Response::new(r))

//         // Err(Status::new(tonic::Code::Ok, "n"))
//     }
//     async fn std_in(&self, request: Request<P9cpuStdInRequest>) -> RpcResult<P9cpuStdInResponse> {
//         Err(Status::new(tonic::Code::Ok, "n"))
//     }

//     async fn std_out(&self, request: Request<P9cpuStdOutRequest>) -> RpcResult<Self::StdOutStream> {
//         Err(Status::new(tonic::Code::Ok, "n"))
//     }
// }
use anyhow::Result;
use async_trait::async_trait;
#[async_trait]
pub trait P9cpuServer {
    async fn serve(&self, net: &str, address: &str) -> Result<()>;
}
