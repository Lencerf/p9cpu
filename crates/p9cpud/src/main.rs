use anyhow::Result;
use flexi_logger::{DeferredNow, Logger, Record};
use libp9cpu::server::P9cpuServerT;

use clap::Parser;

#[derive(clap::ValueEnum, Clone, Debug)]
enum Net {
    Tcp,
    Vsock,
    Unix,
}

#[derive(clap::ValueEnum, Clone, Debug)]
enum Protocol {
    Rpc,
    Ssh
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(long, value_enum, default_value_t = Net::Tcp)]
    net: Net,
    #[arg(long, default_value_t = 17010)]
    port: u32,
    #[arg(long)]
    uds: Option<String>,

    #[arg(long, value_enum, default_value_t = Protocol::Rpc)]
    protocol: Protocol,
}

pub fn log_format(
    w: &mut dyn std::io::Write,
    now: &mut DeferredNow,
    record: &Record,
) -> Result<(), std::io::Error> {
    let level = record.level();
    write!(
        w,
        "[{}] {} [{}]: {}",
        now.format("%Y-%m-%d %H:%M:%S %:z"),
        flexi_logger::style(level).paint(format!("{:<5}", level.to_string())),
        record.module_path().unwrap_or("<unnamed>"),
        &record.args()
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    Logger::try_with_env()?.format(log_format).start()?;
    log::info!("p9cpud started...");
    let args = Args::parse();
    let addr = match args.net {
        Net::Vsock => libp9cpu::Addr::Vsock(tokio_vsock::VsockAddr::new(
            vsock::VMADDR_CID_ANY,
            args.port,
        )),
        Net::Tcp => libp9cpu::Addr::Tcp(format!("[::]:{}", args.port).parse().unwrap()),
        Net::Unix => libp9cpu::Addr::Uds(args.uds.unwrap()),
    };
    match args.protocol {
        Protocol::Rpc => {
            let server = libp9cpu::server::rpc_based();
            server.serve(addr).await?;
        }
        Protocol::Ssh => {
            let server = libp9cpu::server::ssh_based();
            server.serve(addr).await?;
        }
    }
    Ok(())
}
