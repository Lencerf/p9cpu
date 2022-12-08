use std::ffi::CString;
use std::io::{Stdin, Write, Read};
use std::os::unix::prelude::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::Stdio;

use anyhow::{Ok, Result};
use libc::{grantpt, O_NOCTTY};
use libp9cpu::rpc;
use libp9cpu::server::P9cpuServer;
use tokio::io::AsyncWriteExt;

#[tokio::main]
async fn main() -> Result<()> {
// fn main() -> Result<()> {
    // let buf = b"w";
    // let buf2 = b"w2\n";
    // let mut stdout = tokio::io::stdout();
    // stdout.write_all(buf).await.unwrap();
    // stdout.flush().await.unwrap();
    // tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    // stdout.write_all(buf2).await.unwrap();


    let server = rpc::P9cpuServer {};
    server.serve("tcp", "0.0.0.0:8000").await?;

    // // let ptm = tokio::fs::OpenOptions::new().read(true).write(true).open("/dev/ptmx").await?;
    // // let fd = ptm.as_raw_fd();
    // let mut ptm_file;
    // let mut handle;
    // let mut current;
    // unsafe {
    //     let ptm_fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
    //     if ptm_fd < 0 {
    //         println!("fd < 0");
    //         return Ok(());
    //     }
    //     if grantpt(ptm_fd) < 0 {
    //         println!("grantpt fail");
    //         return Ok(());
    //     }
    //     if libc::unlockpt(ptm_fd) < 0 {
    //         println!("unlock pt fail");
    //         return Ok(());
    //     }
    //     let mut buf: Vec<_> = vec![0; 100];
    //     if libc::ptsname_r(ptm_fd, buf.as_mut_ptr() as *mut libc::c_char, 100) < 0 {
    //         println!("ptsname faile");
    //         return Ok(());
    //     }
    //     println!("ptsname = {}", String::from_utf8_unchecked(buf.clone()));
    //     let pts_fd = libc::open(buf.as_ptr() as *const libc::c_char, libc::O_RDWR);
    //     if pts_fd < 0 {
    //         println!("open pts fail");
    //         return Ok(());
    //     }
    //     ptm_file = std::fs::File::from_raw_fd(ptm_fd);
    //     let pts_file = std::fs::File::from_raw_fd(pts_fd);
    //     let mut child =  std::process::Command::new("elvish");
    //     child
    //     .stdin(Stdio::from_raw_fd(pts_fd))
    //     .stdout(Stdio::from_raw_fd(pts_fd))
    //     .stderr(Stdio::from_raw_fd(pts_fd)).pre_exec(|| {
    //         libc::setsid();
    //         libc::ioctl(0, libc::TIOCSCTTY);
    //         Result::Ok(())
    //     });
    //     handle =child.spawn().unwrap();
    //     drop(pts_file);
    //     // println!("spawned");
    //     // ptm_file.write(b"pwd\n exit 1").unwrap();
    //     // println!("write");
    //     // let mut buf = vec![0;100];
    //     // let retlen = ptm_file.read(&mut buf).unwrap();
    //     // buf.truncate(retlen);
    //     // println!("ttyoutput = {:?}", String::from_utf8_unchecked(buf) );
    //     current = nix::sys::termios::tcgetattr(0).unwrap();
    //     let mut raw = current.clone();
    //     nix::sys::termios::cfmakeraw(&mut raw);
    //     nix::sys::termios::tcsetattr(0, nix::sys::termios::SetArg::TCSANOW, &raw).unwrap();
    //     println!("done");
    // }
    // let mut sub_in = ptm_file.try_clone().unwrap();
    // let mut sub_out = ptm_file.try_clone().unwrap();
    // let mut in_thread = std::thread::Builder::new().name("stdin".to_owned()).spawn(move ||{
    //     let mut stdin = std::io::stdin();
    //     let mut buf = [0];
    //     while let Result::Ok(1) = stdin.read(&mut buf) {
    //         if sub_in.write_all(&buf).is_err() {
    //             break;
    //         }
    //     }
    // })?;
    // std::thread::Builder::new().name("stdout".to_owned()).spawn(move || {
    //     let mut stdout = std::io::stdout();
    //     let mut buf = [0];
    //     while let Result::Ok(1) = sub_out.read(&mut buf) {
    //         if stdout.write(&buf).is_err() || stdout.flush().is_err() {
    //             break;
    //         }
    //     }
    // })?.join().unwrap();
    // handle.wait().unwrap();
    // println!("wait in thread");
    // in_thread.join().unwrap();
    // nix::sys::termios::tcsetattr(0, nix::sys::termios::SetArg::TCSANOW, &current)?;
    // // let pts_file = tokio::fs::OpenOptions.

    Ok(())
}
