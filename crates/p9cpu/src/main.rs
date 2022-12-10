use std::{collections::VecDeque, vec};

use libp9cpu::client::P9cpuCommand;

use clap::Parser;

#[derive(clap::ValueEnum, Clone, Debug)]
enum Net {
    Tcp,
    Vsock,
    Unix,
    // UnixVsock,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long, default_value_t = false)]
    tty: bool,

    #[arg(long, value_enum, default_value_t = Net::Tcp)]
    net: Net,

    #[arg(long, default_value_t = 11200)]
    port: u32,

    #[arg(last = true)]
    host_and_args: Vec<String>,
}

async fn app() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.host_and_args.len() < 2 {
        println!("no engouth args");
        return Ok(());
    }
    let host = &args.host_and_args[0];
    let addr = match args.net {
        Net::Vsock => libp9cpu::Addr::Vsock(tokio_vsock::VsockAddr::new(host.parse().unwrap(), args.port)),
        Net::Unix => libp9cpu::Addr::Uds(host.to_owned()),
        Net::Tcp => libp9cpu::Addr::Tcp(format!("{}:{}", host, args.port).parse().unwrap()),
    };

    let mut client = libp9cpu::client::rpc_based(addr).await?;
    let cmd = P9cpuCommand {
        program: args.host_and_args[1].clone(),
        args: args.host_and_args[2..].to_vec(),
        env: vec![],
        namespace: vec![],
        fstab: vec![],
        tty: args.tty,
    };
    client.start(cmd).await?;
    client.wait().await?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let ret = runtime.block_on(app());

    runtime.shutdown_timeout(std::time::Duration::from_secs(0));

    ret
}
