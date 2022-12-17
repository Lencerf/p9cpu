use std::{collections::HashMap, os::unix::prelude::OsStringExt};

use libp9cpu::{fstab::FsTab, EnvVar, P9cpuCommand};
use anyhow::Result;
use clap::Parser;
use tokio::io::AsyncBufReadExt;

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
    #[arg(long, value_enum, default_value_t = Net::Tcp)]
    net: Net,

    #[arg(long, default_value_t = 11200)]
    port: u32,

    #[arg(long, default_value = "")]
    namespace: String,

    #[arg(short, long, default_value_t = false)]
    tty: bool,

    #[arg(long)]
    fs_tab: Option<String>,

    #[arg(long, default_value = "/p9cputmp")]
    tmp_mnt: String,

    #[arg()]
    host: String,

    #[arg()]
    program: Option<String>,

    #[arg(last = true)]
    args: Vec<String>,
}

fn parse_namespace(namespace: &str) -> HashMap<String, String> {
    let mut result = HashMap::new();
    if namespace.is_empty() {
        return result;
    }
    for part in namespace.split(':') {
        let mut iter = part.split('=');
        let Some(target) = iter.next() else {
            println!("invalid namespace: {}", part);
            continue;
        };
        let source = iter.next().unwrap_or("");
        if iter.next().is_some() {
            println!("invalid namespace: {}", part);
            continue;
        }
        if result.contains_key(target) {
            println!("duplicate target: {}", target);
            continue;
        }
        result.insert(target.to_owned(), source.to_owned());
    }
    result
}

async fn app(args: Args) -> Result<()> {
    println!("args = {:?}", args);
    let addr = match args.net {
        Net::Vsock => libp9cpu::Addr::Vsock(tokio_vsock::VsockAddr::new(
            args.host.parse().unwrap(),
            args.port,
        )),
        Net::Unix => libp9cpu::Addr::Uds(args.host),
        Net::Tcp => libp9cpu::Addr::Tcp(format!("{}:{}", args.host, args.port).parse()?),
    };
    let mut client = libp9cpu::client::rpc_based(addr).await?;

    let env_vars: Vec<_> = std::env::vars_os()
        .map(|(k, v)| EnvVar {
            key: k.into_vec(),
            val: v.into_vec(),
        })
        .collect();

    let mut fs_tab_lines = vec![];
    if let Some(ref fs_tab) = args.fs_tab {
        let fs_tab_file = tokio::fs::File::open(fs_tab).await?;
        let mut lines = tokio::io::BufReader::new(fs_tab_file).lines();
        while let Some(line) = lines.next_line().await? {
            if line.starts_with('#') {
                continue;
            }
            fs_tab_lines.push(FsTab::try_from(line.as_str())?);
        }
    }
    let cmd = P9cpuCommand {
        program: args.program.unwrap(),
        args: args.args,
        env: env_vars,
        namespace: parse_namespace(&args.namespace),
        fstab: fs_tab_lines,
        tty: args.tty,
        tmp_mnt: if !args.tmp_mnt.is_empty() {
            Some(args.tmp_mnt)
        } else {
            None
        },
    };
    client.start(cmd).await?;
    client.wait().await?;
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let ret = runtime.block_on(app(args));

    runtime.shutdown_timeout(std::time::Duration::from_secs(0));

    ret
}
