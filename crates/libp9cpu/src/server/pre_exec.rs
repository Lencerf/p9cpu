use super::{MountParams, P9cpuServerError};
use crate::fstab::FsTab;
use nix::mount::MsFlags;
use std::{
    collections::HashMap,
    ffi::{CStr, CString},
};
use tokio::process::Command;

macro_rules! check_ret {
    ($ret:expr) => {
        if $ret != 0 {
            return Err(std::io::Error::last_os_error());
        }
    };
}

fn get_ptr(s: &Option<CString>) -> *const libc::c_char {
    match s {
        Some(s) => s.as_ptr(),
        None => std::ptr::null(),
    }
}

fn try_mkdir(path: &CStr) -> std::io::Result<()> {
    let ret = unsafe { libc::mkdir(path.as_ptr() as *const libc::c_char, 0o777) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::AlreadyExists {
            Ok(())
        } else {
            Err(err)
        }
    } else {
        Ok(())
    }
}

pub fn mount_fstab(cmd: &mut Command, fstab: Vec<FsTab>) -> Result<(), P9cpuServerError> {
    let mut mount_params = vec![];
    for tab in fstab {
        mount_params.push(MountParams::try_from(tab)?);
    }
    let mount_closure = move || {
        for param in &mount_params {
            try_mkdir(&param.target)?;
            let ret = unsafe {
                libc::mount(
                    get_ptr(&param.source),
                    param.target.as_ptr(),
                    get_ptr(&param.fstype),
                    param.flags.bits(),
                    get_ptr(&param.data) as *const libc::c_void,
                )
            };
            check_ret!(ret);
        }
        Ok(())
    };
    unsafe {
        cmd.pre_exec(mount_closure);
    };
    Ok(())
}

pub fn create_private_root(cmd: &mut Command) {
    let op = move || {
        let ret = unsafe { libc::unshare(libc::CLONE_NEWNS) };
        check_ret!(ret);
        let ret = unsafe {
            libc::mount(
                std::ptr::null(),
                "/\0".as_ptr() as *const libc::c_char,
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            )
        };
        check_ret!(ret);
        Ok(())
    };
    unsafe {
        cmd.pre_exec(op);
    }
}

pub fn create_namespace_9p(
    cmd: &mut Command,
    namespace: HashMap<String, String>,
    tmp_mnt: String,
    ninep_port: u16,
    user: &str,
) -> Result<(), P9cpuServerError> {
    let c_str_op = P9cpuServerError::StringContainsNull;
    let ninep_mount = format!("{}/mnt9p", &tmp_mnt);
    let tmp_mnt_c = CString::new(tmp_mnt).map_err(c_str_op)?;
    let ninp_mount_c = CString::new(ninep_mount.clone()).map_err(c_str_op)?;
    let mut mount_params = vec![];
    mount_params.push(MountParams {
        source: Some(CString::new("p9cpu").unwrap()),
        target: tmp_mnt_c,
        fstype: Some(CString::new("tmpfs").unwrap()),
        flags: MsFlags::empty(),
        data: None,
    });
    let ninep_opt = format!(
        "version=9p2000.L,trans=tcp,port={},uname={}",
        ninep_port, user
    );
    let ninp_opt_c = CString::new(ninep_opt).map_err(c_str_op)?;
    mount_params.push(MountParams {
        source: Some(CString::new("127.0.0.1").unwrap()),
        target: ninp_mount_c,
        fstype: Some(CString::new("9p").unwrap()),
        flags: MsFlags::MS_NODEV | MsFlags::MS_NOSUID,
        data: Some(ninp_opt_c),
    });
    for (target, mut source) in namespace.iter() {
        if source.is_empty() {
            source = target;
        }
        let local_source = format!("{}{}", &ninep_mount, source);
        mount_params.push(MountParams {
            source: Some(CString::new(local_source).unwrap()),
            target: CString::new(target.to_owned()).unwrap(),
            fstype: None,
            flags: MsFlags::MS_BIND,
            data: None,
        });
    }

    let op = move || {
        for param in &mount_params {
            let ret = unsafe { libc::mkdir(param.target.as_ptr() as *const libc::c_char, 0o777) };
            check_ret!(ret);
            let ret = unsafe {
                libc::mount(
                    get_ptr(&param.source),
                    param.target.as_ptr(),
                    get_ptr(&param.fstype),
                    param.flags.bits(),
                    get_ptr(&param.data) as *const libc::c_void,
                )
            };
            check_ret!(ret);
        }
        Ok(())
    };
    unsafe {
        cmd.pre_exec(op);
    }
    Ok(())
}
