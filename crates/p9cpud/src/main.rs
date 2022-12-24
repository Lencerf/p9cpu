use anyhow::{Result};
use flexi_logger::{Logger, DeferredNow, Record};
use libp9cpu::server::P9cpuServerT;

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
    #[arg(long, default_value_t = 17010)]
    port: u32,
    #[arg(long)]
    uds: Option<String>,
}

pub fn log_format(
    w: &mut dyn std::io::Write,
    now: &mut DeferredNow,
    record: &Record,
) -> Result<(), std::io::Error> {
    let level = record.level();
    write!(
        w,
        "[{}] {:<5} [{}]: {}",
        now.format("%Y-%m-%d %H:%M:%S %:z"),
        flexi_logger::style(level).paint(level.to_string()),
        record.module_path().unwrap_or("<unnamed>"),
        &record.args()
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    Logger::try_with_env()?.format(log_format).start()?;
    log::error!("f");
    log::info!("p9cpud started...");
    log::warn!("dd");
    log::debug!("da");
    log::trace!("f");
    let args = Args::parse();
    let server = libp9cpu::server::rpc_based();
    let addr = match args.net {
        Net::Vsock => libp9cpu::Addr::Vsock(tokio_vsock::VsockAddr::new(
            vsock::VMADDR_CID_ANY,
            args.port,
        )),
        Net::Tcp => libp9cpu::Addr::Tcp(format!("[::]:{}", args.port).parse().unwrap()),
        Net::Unix => libp9cpu::Addr::Uds(args.uds.unwrap()),
    };
    server.serve(addr).await?;
    Ok(())
}
