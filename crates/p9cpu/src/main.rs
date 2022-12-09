use std::vec;

use libp9cpu::client::P9cpuCommand;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long, default_value_t = false)]
    tty: bool,

    #[arg(last = true)]
    cmd_args: Vec<String>,
}

async fn app() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let mut client = libp9cpu::client::rpc_based("http://0.0.0.0:8000").await?;
    let cmd = P9cpuCommand {
        program: args.cmd_args[0].clone(),
        args: args.cmd_args[1..].to_vec(),
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
