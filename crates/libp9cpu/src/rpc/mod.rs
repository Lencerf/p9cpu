use anyhow::Result;

use futures::Stream;

use tonic::Status;

pub mod rpc_client;
pub mod rpc_server;

tonic::include_proto!("p9cpu");

// impl<'a, T, E> AsBytes<'a> for Result<T, E>
// where
//     T: AsBytes<'a>,
// {
//     fn as_bytes(&self) -> &[u8] {
//         match self {
//             Ok(data) => data.as_bytes(),
//             Err(_) => &[],
//         }
//     }
// }

// impl AsBytes<'_> for P9cpuBytes {
//     fn as_bytes(&self) -> &[u8] {
//         self.data.as_slice()
//     }
// }

impl From<P9cpuBytes> for Vec<u8> {
    fn from(b: P9cpuBytes) -> Self {
        b.data
    }
}

impl From<u8> for TtyinRequest {
    fn from(byte: u8) -> Self {
        TtyinRequest {
            id: None,
            byte: byte as u32,
        }
    }
}

// impl AsBytes<'_> for P9cpuStdinRequest {
//     fn as_bytes(&self) -> &[u8] {
//         self.data.as_slice()
//     }
// }

// impl AsBytes<'_> for NinepForwardRequest {
//     fn as_bytes(&self) -> &[u8] {
//         self.data.as_slice()
//     }
// }

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

// impl FromVecu8 for Result<P9cpuBytes, Status> {
//     fn from_vec_u8(vec: Vec<u8>) -> Self {
//         Ok(P9cpuBytes { data: vec })
//     }
// }

// impl<T, E> IntoByteVec for Result<T, E>
// where
//     T: IntoByteVec,
// {
//     fn into_byte_vec(self) -> Vec<u8> {
//         match self {
//             Ok(inner) => inner.into_byte_vec(),
//             Err(_) => vec![],
//         }
//     }
// }

// impl IntoByteVec for P9cpuBytes {
//     fn into_byte_vec(self) -> Vec<u8> {
//         self.data
//     }
// }

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
