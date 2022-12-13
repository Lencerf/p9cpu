pub mod client;
pub mod fstab;
pub mod rpc;
pub mod server;

pub type P9cpuCommand = crate::rpc::P9cpuCommand;
pub type EnvVar = crate::rpc::EnvVar;

#[derive(Debug)]
pub enum Addr {
    Tcp(std::net::SocketAddr),
    Vsock(tokio_vsock::VsockAddr),
    Uds(String),
}

pub trait AsBytes<'a> {
    fn as_bytes(&self) -> &[u8];
}

pub trait FromVecu8 {
    fn from_vec_u8(vec: Vec<u8>) -> Self;
}

pub trait IntoByteVec {
    fn into_byte_vec(self) -> Vec<u8>;
}
