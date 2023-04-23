use std::collections::HashSet;

pub mod client;
pub mod cmd;
mod rpc;
pub mod server;
pub mod ssh;

// pub type Command = crate::cmd::Command;
// pub type EnvVar = crate::cmd::EnvVar;

#[derive(Debug)]
pub enum Addr {
    Tcp(std::net::SocketAddr),
    Vsock(tokio_vsock::VsockAddr),
    Uds(String),
}

pub const NINEP_MOUNT: &str = "mnt9p";
pub const LOCAL_ROOT_MOUNT: &str = "local";


pub fn parse_namespace(namespace: &str, tmp_mnt: &str) -> Vec<cmd::FsTab> {
    let mut result = vec![];
    if namespace.is_empty() {
        return result;
    }
    let mut targets = HashSet::new();
    for part in namespace.split(':') {
        let mut iter = part.split('=');
        let Some(target) = iter.next() else {
                    println!("invalid namespace: {}", part);
                    continue;
                };
        let source = iter.next().unwrap_or(target);
        if iter.next().is_some() {
            println!("invalid namespace: {}", part);
            continue;
        }
        if targets.contains(target) {
            println!("duplicate target: {}", target);
            continue;
        }
        targets.insert(target);
        result.push(cmd::FsTab {
            spec: format!("{}/{}{}", tmp_mnt, NINEP_MOUNT, source),
            file: target.to_owned(),
            vfstype: "none".to_owned(),
            mntops: "defaults,bind".to_owned(),
            freq: 0,
            passno: 0,
        });
    }
    result
}

// pub trait AsBytes<'a> {
//     fn as_bytes(&self) -> &[u8];
// }

// pub trait FromVecu8 {
//     fn from_vec_u8(vec: Vec<u8>) -> Self;
// }

// pub trait IntoByteVec {
//     fn into_byte_vec(self) -> Vec<u8>;
// }
