use anyhow::Result;

use futures::Stream;

use tonic::Status;

use crate::{AsBytes, FromVecu8};

pub mod rpc_client;
pub mod rpc_server;

tonic::include_proto!("p9cpu");

impl AsBytes<'_> for Result<P9cpuBytes, Status> {
    fn as_bytes(&self) -> &[u8] {
        match self {
            Ok(bytes) => bytes.data.as_slice(),
            Err(_e) => &[],
        }
    }
}

impl AsBytes<'_> for Result<P9cpuStdinRequest, Status> {
    fn as_bytes(&self) -> &[u8] {
        match self {
            Ok(req) => req.data.as_slice(),
            Err(_e) => &[],
        }
    }
}

impl From<Vec<u8>> for P9cpuStdinRequest {
    fn from(data: Vec<u8>) -> Self {
        P9cpuStdinRequest { id: None, data }
    }
}

impl FromVecu8 for Result<P9cpuBytes, Status> {
    fn from_vec_u8(vec: Vec<u8>) -> Self {
        Ok(P9cpuBytes { data: vec })
    }
}

struct PrependedStream<I, S> {
    stream: S,
    item: Option<I>,
}

impl<I, S> Stream for PrependedStream<I, S>
where
    S: Stream<Item = I> + Unpin,
    I: Unpin,
{
    type Item = I;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if let Some(item) = self.as_mut().item.take() {
            std::task::Poll::Ready(Some(item))
        } else {
            S::poll_next(std::pin::Pin::new(&mut self.get_mut().stream), cx)
        }
    }
}
