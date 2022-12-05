use std::vec;

use libp9cpu::client::P9cpuCommand;
use libp9cpu::rpc::{P9cpuClient};

use clap::Parser;

/// p9cpu client
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Name of the person to greet
    #[arg(short, long)]
    name: String,

    /// Number of times to greet
    #[arg(short, long, default_value_t = 1)]
    count: u8,

    #[arg(last = true)]
    cmd_args: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let mut client = P9cpuClient::new("tcp", "http://0.0.0.0:8000").await?;
    let cmd = P9cpuCommand {
        program: args.cmd_args[0].clone(),
        args: args.cmd_args[1..].to_vec(),
        env: vec![],
        namespace: vec![],
        fstab: vec![],
    };
    // println!("{:?}", cmd);
    let sid = client.start(cmd).await?;
    client.wait(sid).await?;

    // let stdout_request = P9cpuStdoutRequest {
    //     id: r.id,
    //     len: 0
    // };
    // let mut stream = client.stdout_st(stdout_request).await?.into_inner();
    // while let Some(result) =  stream.next().await {
    //     match result {
    //         Ok(resp) => println!("{}", String::from_utf8_lossy(&resp.data)),
    //         Err(e) => println!("error = {:?}", e),
    //     }
    // }
    Ok(())
}
