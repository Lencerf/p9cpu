

use futures::Stream;

pub(crate) mod rpc_client;
pub(crate) mod rpc_server;

tonic::include_proto!("p9cpu");

impl From<P9cpuBytes> for Vec<u8> {
    fn from(b: P9cpuBytes) -> Self {
        b.data
    }
}


impl From<Vec<u8>> for P9cpuStdinRequest {
    fn from(data: Vec<u8>) -> Self {
        P9cpuStdinRequest { id: None, data }
    }
}

impl From<Vec<u8>> for NinepForwardRequest {
    fn from(data: Vec<u8>) -> Self {
        NinepForwardRequest { id: None, data }
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
