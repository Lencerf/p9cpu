use anyhow::{Ok, Result};


use libp9cpu::server::P9cpuServerT;

#[tokio::main]
async fn main() -> Result<()> {
    let server = libp9cpu::server::rpc_based();
    server.serve("tcp", "0.0.0.0:8000").await?;
    Ok(())
}
