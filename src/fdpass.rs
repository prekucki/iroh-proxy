//! OpenSSH `ProxyUseFdpass` support.
//!
//! In fdpass mode, OpenSSH creates a `socketpair(AF_UNIX, SOCK_STREAM)` and
//! execs the ProxyCommand with stdout (fd 1) as one end of the pair. The
//! ProxyCommand is expected to:
//!   1. produce a connected socket,
//!   2. send it back to ssh via `sendmsg`/`SCM_RIGHTS` on fd 1,
//!   3. exit (ssh `waitpid()`s the ProxyCommand before using the fd).
//!
//! Because the ProxyCommand must exit, we hand the other end of an internal
//! socketpair to a detached child `iroh-proxy forward --fdpass-fd <N>` that
//! performs the actual iroh relay. This is more efficient than piping bytes
//! through the parent's stdio: ssh's traffic goes directly over the kernel
//! socket to the relay process.

#![cfg(unix)]

use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use iroh::SecretKey;

use crate::proxy::{build_endpoint, connect_remote, pump_streams};
use crate::remote_path::RemotePath;

/// Parent side: invoked by ssh. Create a socketpair, spawn a detached child
/// holding one end, send the other end back to ssh on fd 1, and exit.
pub fn run_fdpass_parent(remote_arg: &str, key_file: Option<&PathBuf>) -> Result<()> {
    let (for_ssh, for_child) = unix_socketpair().context("socketpair failed")?;

    let exe = std::env::current_exe().context("failed to resolve current executable path")?;
    let mut cmd = Command::new(exe);
    if let Some(k) = key_file {
        cmd.arg("--key-file").arg(k);
    }
    cmd.arg("forward")
        .arg("--fdpass-fd")
        .arg("3")
        .arg(remote_arg);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let child_raw = for_child.into_raw_fd();
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(move || {
            if libc::dup2(child_raw, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let flags = libc::fcntl(3, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(3, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            }
            if libc::setsid() < 0 {
                // not fatal: just means we're already a session leader
            }
            Ok(())
        });
    }

    let _child = cmd
        .spawn()
        .context("failed to spawn detached fdpass relay child")?;
    // We intentionally leak the Child handle: once the parent exits, init
    // reaps the detached relay process.
    std::mem::forget(_child);

    // Safety: child_raw was dup2'd into the child's fd 3; the duplicate still
    // lives in *our* process as child_raw. Close it so we don't hold a
    // spurious reference.
    unsafe {
        libc::close(child_raw);
    }

    send_fd(libc::STDOUT_FILENO, for_ssh.as_raw_fd())
        .context("failed to send connected fd back to ssh via SCM_RIGHTS on stdout")?;

    Ok(())
}

/// Child side: wrap the passed fd as a Unix stream and forward to iroh.
pub async fn run_fdpass_child(
    secret_key: SecretKey,
    remote: RemotePath,
    raw_fd: RawFd,
) -> Result<()> {
    // Take ownership of the inherited fd.
    let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(raw_fd) };
    std_stream
        .set_nonblocking(true)
        .context("failed to set inherited fd non-blocking")?;
    let stream = tokio::net::UnixStream::from_std(std_stream)
        .context("failed to register inherited fd with tokio")?;

    let endpoint = build_endpoint(secret_key, false).await?;
    let conn = connect_remote(&endpoint, &remote).await?;
    let (send, recv) = conn.open_bi().await?;

    let (local_read, local_write) = stream.into_split();
    // fdpass relay: run both directions to completion (no close-on-request timeout).
    let result = pump_streams(local_read, local_write, send, recv, None).await;
    conn.close(0u32.into(), b"closed");
    result?;
    Ok(())
}

fn unix_socketpair() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Send a single file descriptor over an AF_UNIX SOCK_STREAM socket using
/// SCM_RIGHTS. Matches the protocol OpenSSH's `ProxyUseFdpass` expects.
fn send_fd(sock: RawFd, fd: RawFd) -> std::io::Result<()> {
    use std::mem;

    // Need at least one byte of data — some kernels reject cmsg-only messages.
    let mut data: u8 = 0;
    let mut iov = libc::iovec {
        iov_base: &mut data as *mut _ as *mut libc::c_void,
        iov_len: 1,
    };

    let fd_size = mem::size_of::<RawFd>() as libc::c_uint;
    let cmsg_space = unsafe { libc::CMSG_SPACE(fd_size) } as usize;
    let mut cmsg_buf = vec![0u8; cmsg_space];

    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cmsg_space as _;

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(std::io::Error::other("CMSG_FIRSTHDR returned null"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(fd_size) as _;
        std::ptr::copy_nonoverlapping(&fd, libc::CMSG_DATA(cmsg) as *mut RawFd, 1);
    }

    loop {
        let rc = unsafe { libc::sendmsg(sock, &msg, 0) };
        if rc >= 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
}

pub fn fdpass_usage_error() -> anyhow::Error {
    anyhow::anyhow!(
        "`forward --fdpass` takes exactly one argument: the remote path \
         (<endpoint-id>/tcp/<name>); it cannot be combined with a local listen address"
    )
}
