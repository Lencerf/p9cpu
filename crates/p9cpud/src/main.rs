use anyhow::Result;
use libp9cpu::rpc;
use libp9cpu::server::P9cpuServer;

#[tokio::main]
async fn main() -> Result<()> {
    let server = rpc::P9cpuServer {};
    server.serve("tcp", "0.0.0.0:8000").await
}
