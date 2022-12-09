pub mod client;
pub mod rpc;
pub mod server;

pub trait AsBytes<'a> {
    fn as_bytes(&self) -> &[u8];
}

pub trait FromVecu8 {
    fn from_vec_u8(vec: Vec<u8>) -> Self;
}
