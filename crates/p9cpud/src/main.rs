use anyhow::{Ok, Result};
use libp9cpu::server::P9cpuServerT;
use std::net::{SocketAddr, SocketAddrV6};

use clap::Parser;

#[derive(clap::ValueEnum, Clone, Debug)]
enum Net {
    Tcp,
    Vsock,
    Unix,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(long, value_enum, default_value_t = Net::Tcp)]
    net: Net,
    #[arg(long, default_value_t = 11200)]
    port: u32,
    #[arg(long, default_value = "")]
    uds: String,

    #[arg(last = true)]
    cmd_args: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let server = libp9cpu::server::rpc_based();
    let addr = match args.net {
        Net::Vsock => libp9cpu::Addr::Vsock(tokio_vsock::VsockAddr::new(
            vsock::VMADDR_CID_ANY,
            args.port,
        )),
        Net::Tcp => libp9cpu::Addr::Tcp(format!("[::]:{}", args.port).parse().unwrap()),
        Net::Unix => libp9cpu::Addr::Uds(args.uds),
    };
    server.serve(addr).await?;
    Ok(())
}
