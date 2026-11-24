// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! Shared helpers used by every module.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// HTTP port (Python: PORT = 8467). Override with WBS_PORT for side-by-side testing.
pub fn port() -> u16 {
    std::env::var("WBS_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(8467)
}

/// MXL shared-memory root (Python: SHM_BASE_DIR = "/dev/shm/mxl"). Linux keeps the rings
/// in RAM under /dev/shm; macOS and Windows have no /dev/shm, so the rings are memory-mapped
/// files in the temp folder (the OS page cache holds them). Override with WBS_SHM_BASE.
pub fn shm_base() -> String {
    std::env::var("WBS_SHM_BASE").unwrap_or_else(|_| {
        if cfg!(target_os = "linux") {
            "/dev/shm/mxl".to_string()
        } else {
            std::env::temp_dir().join("mxl").display().to_string()
        }
    })
}

/// Process-wide shutdown flag (Python: SHUTDOWN_EVENT).
pub fn shutdown_flag() -> Arc<AtomicBool> {
    static FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    FLAG.get_or_init(|| Arc::new(AtomicBool::new(false))).clone()
}

pub fn is_shutting_down() -> bool {
    shutdown_flag().load(Ordering::Relaxed)
}

/// Run a command and return stdout (lossy UTF-8) if it exits within `timeout`,
/// or None on spawn failure / timeout. Stderr is discarded. Exit status is ignored
/// (like Python's subprocess.run(...).stdout).
pub fn run_cmd(args: &[&str], timeout: Duration) -> Option<String> {
    run_cmd_full(args, timeout).map(|(out, _code)| out)
}

/// Like run_cmd but also returns the exit code (-1 if killed by a signal).
pub fn run_cmd_full(args: &[&str], timeout: Duration) -> Option<(String, i32)> {
    let (prog, rest) = args.split_first()?;
    let mut child = match Command::new(prog)
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            wbs_log::command_failed(args, -1, &e.to_string());
            return None;
        }
    };
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let buf = reader.join().unwrap_or_default();
                return Some((String::from_utf8_lossy(&buf).into_owned(), status.code().unwrap_or(-1)));
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    wbs_log::command_failed(args, -2, &format!("timed out after {:.1} s", timeout.as_secs_f64()));
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
}

/// True if `prog` is on PATH (Python: shutil.which).
pub fn which(prog: &str) -> bool {
    let exe = format!("{prog}{}", std::env::consts::EXE_SUFFIX);
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(prog).is_file() || d.join(&exe).is_file()))
        .unwrap_or(false)
}

/// Seconds since the Unix epoch as f64 (Python: time.time()).
pub fn now_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Linear amplitude to dBFS, floored at -120 like the Python meters.
pub fn to_db(v: f64) -> f64 {
    if v > 1e-6 { round_to(20.0 * v.log10(), 2) } else { -120.0 }
}

/// Python round(x, n).
pub fn round_to(x: f64, n: i32) -> f64 {
    let m = 10f64.powi(n);
    (x * m).round() / m
}

/// New audio of one flow since a caller's cursor (GET /api/mxl/flow/<id>/pcm).
/// `payload` is planar little-endian Float32: all of ch0's frames, then ch1, ...
#[derive(Debug, Clone, Default)]
pub struct PcmChunk {
    pub payload: Vec<u8>,
    pub cursor: i64,
    pub channels: usize,
    pub sample_rate: u32,
    pub frames: usize,
}

// ---------------------------------------------------------------------------
// Guards, dry run and the state lock (Python STATE_LOCK), shared by every application
// ---------------------------------------------------------------------------
use std::sync::{Condvar, Mutex};
use std::thread::{self, ThreadId};

pub fn guard<T>(f: impl FnOnce() -> T) -> Option<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).ok()
}

/// WBS_DRY_RUN=1: skip every side effect on the live audio graph (see module docs).
pub fn dry_run() -> bool {
    std::env::var("WBS_DRY_RUN").map(|v| v == "1").unwrap_or(false)
}

// ---------------------------------------------------------------------------
// STATE_LOCK (re-entrant, like threading.RLock)
// ---------------------------------------------------------------------------
pub struct RLock {
    st: Mutex<(Option<ThreadId>, usize)>,
    cv: Condvar,
}

pub struct RLockGuard<'a>(&'a RLock);

impl RLock {
    pub const fn new() -> Self {
        RLock { st: Mutex::new((None, 0)), cv: Condvar::new() }
    }
    pub fn lock(&self) -> RLockGuard<'_> {
        let me = thread::current().id();
        let mut g = self.st.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            match g.0 {
                None => {
                    *g = (Some(me), 1);
                    break;
                }
                Some(owner) if owner == me => {
                    g.1 += 1;
                    break;
                }
                _ => g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner()),
            }
        }
        RLockGuard(self)
    }
}

impl Default for RLock {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for RLockGuard<'_> {
    fn drop(&mut self) {
        let mut g = self.0.st.lock().unwrap_or_else(|e| e.into_inner());
        g.1 = g.1.saturating_sub(1);
        if g.1 == 0 {
            g.0 = None;
            self.0.cv.notify_all();
        }
    }
}

static STATE_LOCK: RLock = RLock::new();

/// Python `with STATE_LOCK:`.
pub fn state_lock() -> RLockGuard<'static> {
    STATE_LOCK.lock()
}


/// TAI leap seconds offset (SMPTE 2059-1): TAI is currently UTC + 37 seconds.
pub const TAI_OFFSET_SEC: f64 = 37.0;

/// Nanoseconds on the TAI timescale (Python: current_tai_ns, UTC + 37 s).
pub fn current_tai_ns() -> u64 {
    ((now_f64() + TAI_OFFSET_SEC) * 1_000_000_000.0) as u64
}

/// pactl and the other audio-graph commands: 2.5 s timeout, (code, stdout, stderr).
pub mod pactl {
    use super::dry_run;
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    pub fn run_cmd(args: &[&str]) -> (i32, String, String) {
        let Some((prog, rest)) = args.split_first() else { return (-1, String::new(), "empty command".into()) };
        let mut child = match Command::new(prog)
            .args(rest)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                wbs_log::command_failed(args, -1, &e.to_string());
                return (-1, String::new(), e.to_string());
            }
        };
        let mut so = child.stdout.take();
        let mut se = child.stderr.take();
        let ro = thread::spawn(move || {
            let mut b = Vec::new();
            if let Some(s) = so.as_mut() {
                let _ = s.read_to_end(&mut b);
            }
            b
        });
        let re = thread::spawn(move || {
            let mut b = Vec::new();
            if let Some(s) = se.as_mut() {
                let _ = s.read_to_end(&mut b);
            }
            b
        });
        let start = Instant::now();
        let timeout = Duration::from_millis(2500);
        loop {
            match child.try_wait() {
                Ok(Some(st)) => {
                    let o = ro.join().unwrap_or_default();
                    let e = re.join().unwrap_or_default();
                    let (code, err) = (st.code().unwrap_or(-1), String::from_utf8_lossy(&e).trim().to_string());
                    if code != 0 {
                        wbs_log::command_failed(args, code, &err);
                    }
                    return (code, String::from_utf8_lossy(&o).trim().to_string(), err);
                }
                Ok(None) if start.elapsed() >= timeout => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return (-1, String::new(), format!("Command '{:?}' timed out after 2.5 seconds", args));
                }
                Ok(None) => thread::sleep(Duration::from_millis(5)),
                Err(e) => return (-1, String::new(), e.to_string()),
            }
        }
    }

    /// run_cmd for commands that change the live audio graph (no-op under WBS_DRY_RUN).
    pub fn run_mut(args: &[&str]) -> (i32, String, String) {
        if dry_run() {
            println!("[DRY RUN] {}", args.join(" "));
            return (-1, String::new(), "dry run".into());
        }
        run_cmd(args)
    }
}
