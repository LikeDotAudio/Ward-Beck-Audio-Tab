// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.com - Ward-Beck.ca). All rights reserved.
//! The few operating-system calls the Tap makes, one place per OS: signalling its own
//! child processes, starting them in their own session, file locks on MXL rings,
//! symlinks, SO_REUSEPORT and the interface list. Linux and macOS use the POSIX calls;
//! Windows has its own equivalent, or does without and says so in the log.

use std::fs::File;
use std::io;
use std::process::Command;
#[cfg(windows)]
use std::process::Stdio;

pub const SIGTERM: i32 = 15;
pub const SIGKILL: i32 = 9;

/// Signal one of the Tap's own children. Windows has no SIGTERM for console programs:
/// both signals end the process tree (taskkill /T /F).
pub fn kill(pid: u32, sig: i32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: plain kill(2) on a pid.
        unsafe { libc::kill(pid as libc::pid_t, sig) == 0 }
    }
    #[cfg(windows)]
    {
        let _ = sig;
        Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

/// Start a child in its own session / process group, so stopping it never signals the Tap.
pub fn new_session(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe (Python: preexec_fn=os.setsid).
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
}

/// The user id `ps -u` takes (empty on Windows, which has no ps).
pub fn uid() -> String {
    #[cfg(unix)]
    {
        unsafe { libc::getuid() }.to_string()
    }
    #[cfg(windows)]
    {
        String::new()
    }
}

/// Exclusive, non-blocking lock on an MXL ring file (flock on Unix, LockFileEx on Windows).
pub fn try_lock_exclusive(f: &File) -> bool {
    f.try_lock().is_ok()
}

pub fn unlock(f: &File) {
    let _ = f.unlock();
}

/// A convenience symlink; Windows needs developer mode or admin for one, so it is skipped there.
pub fn symlink(target: &str, link: &str) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        let _ = (target, link);
        Err(io::Error::new(io::ErrorKind::Unsupported, "symlinks are skipped on Windows"))
    }
}

/// SO_REUSEPORT where it exists (Windows shares a multicast port through SO_REUSEADDR alone).
pub fn reuse_port(sock: &socket2::Socket) {
    #[cfg(unix)]
    {
        let _ = sock.set_reuse_port(true);
    }
    #[cfg(windows)]
    {
        let _ = sock;
    }
}

/// [(index, name)] of the network interfaces.
pub fn interfaces() -> Vec<(u32, String)> {
    apkaudio_ptp::net::list_interfaces()
        .unwrap_or_default()
        .into_iter()
        .map(|(name, index, _, _)| (index, name))
        .collect()
}
