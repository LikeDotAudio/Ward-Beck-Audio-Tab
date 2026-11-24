// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! The Tap's log: one line per event, to the console, to a rotating file and to an
//! in-memory tail served at /api/log. It is meant to be read by someone who was not
//! there, on an OS nobody here can test: every client that connects, every command that
//! fails (with its exit code and stderr), every device or socket that opens or will not.
//!
//! Files: Linux ~/.local/state/ward-beck-audio-tap/tap.log, macOS
//! ~/Library/Logs/Ward-Beck Audio Tap/tap.log, Windows
//! %LOCALAPPDATA%\Ward-Beck\Audio Tap\logs\tap.log. Rotated to tap.log.1 at 5 MB.

use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TAIL: usize = 3000;
const ROTATE_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    fn tag(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO ",
            Level::Warn => "WARN ",
            Level::Error => "ERROR",
        }
    }
}

struct Sink {
    file: Option<File>,
    path: PathBuf,
    written: u64,
    tail: VecDeque<(u64, String)>,
    seq: u64,
    /// Once-per-key messages and their last time (rate limiting repeats).
    last: HashMap<String, Instant>,
}

fn sink() -> &'static Mutex<Sink> {
    static S: OnceLock<Mutex<Sink>> = OnceLock::new();
    S.get_or_init(|| {
        let path = log_path();
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let file = OpenOptions::new().create(true).append(true).open(&path).ok();
        let written = file.as_ref().and_then(|f| f.metadata().ok()).map(|m| m.len()).unwrap_or(0);
        Mutex::new(Sink { file, path, written, tail: VecDeque::new(), seq: 0, last: HashMap::new() })
    })
}

/// Where this OS keeps per-user logs.
pub fn log_path() -> PathBuf {
    if let Ok(p) = std::env::var("WBS_LOG_FILE") {
        return PathBuf::from(p);
    }
    let home = || std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    if cfg!(windows) {
        let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        base.join("Ward-Beck").join("Audio Tap").join("logs").join("tap.log")
    } else if cfg!(target_os = "macos") {
        home().join("Library").join("Logs").join("Ward-Beck Audio Tap").join("tap.log")
    } else {
        let state = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".local").join("state"));
        state.join("ward-beck-audio-tap").join("tap.log")
    }
}

/// UTC time as 2026-11-24T03:12:45.123Z (no date crate).
pub fn timestamp() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (days, sod) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60,
        d.subsec_millis()
    )
}

/// Write one line: `2026-11-24T03:12:45.123Z WARN  [ptp] message`.
pub fn log(level: Level, area: &str, msg: &str) {
    let line = format!("{} {} [{area}] {msg}", timestamp(), level.tag());
    if level >= Level::Warn {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
    let mut s = sink().lock().unwrap_or_else(|p| p.into_inner());
    s.seq += 1;
    let seq = s.seq;
    s.tail.push_back((seq, line.clone()));
    while s.tail.len() > TAIL {
        s.tail.pop_front();
    }
    if s.written > ROTATE_BYTES {
        let rotated = s.path.with_extension("log.1");
        let _ = fs::rename(&s.path, rotated);
        s.file = OpenOptions::new().create(true).append(true).open(&s.path).ok();
        s.written = 0;
    }
    if let Some(f) = s.file.as_mut() {
        let _ = writeln!(f, "{line}");
        s.written += line.len() as u64 + 1;
    }
}

/// Log at most once per `every` for the same key (a failing poll must not flood the file).
pub fn log_limited(level: Level, area: &str, key: &str, every: Duration, msg: &str) {
    {
        let mut s = sink().lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        if s.last.get(key).is_some_and(|t| now.duration_since(*t) < every) {
            return;
        }
        s.last.insert(key.to_string(), now);
    }
    log(level, area, msg);
}

pub fn info(area: &str, msg: &str) {
    log(Level::Info, area, msg)
}
pub fn warn(area: &str, msg: &str) {
    log(Level::Warn, area, msg)
}
pub fn error(area: &str, msg: &str) {
    log(Level::Error, area, msg)
}

/// Lines after sequence number `since` (GET /api/log?since=N), oldest first.
pub fn tail_since(since: u64, max: usize) -> (u64, Vec<String>) {
    let s = sink().lock().unwrap_or_else(|p| p.into_inner());
    let lines: Vec<String> = s.tail.iter().filter(|(n, _)| *n > since).map(|(_, l)| l.clone()).collect();
    let skip = lines.len().saturating_sub(max);
    (s.seq, lines.into_iter().skip(skip).collect())
}

/// The last `n` WARN / ERROR lines.
pub fn recent_problems(n: usize) -> Vec<String> {
    let s = sink().lock().unwrap_or_else(|p| p.into_inner());
    let v: Vec<String> = s.tail.iter().filter(|(_, l)| l.contains(" WARN  [") || l.contains(" ERROR [")).map(|(_, l)| l.clone()).collect();
    v[v.len().saturating_sub(n)..].to_vec()
}

// ---------------------------------------------------------------------------
// Clients: who connects, from where, and what they ask for
// ---------------------------------------------------------------------------

struct Client {
    first: String,
    last: Instant,
    requests: u64,
    paths: BTreeMap<String, u64>,
}

fn clients() -> &'static Mutex<BTreeMap<String, Client>> {
    static C: OnceLock<Mutex<BTreeMap<String, Client>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Count a request. A client (IP + page origin) seen for the first time, or back after a
/// minute of silence, is logged; the rest go into the per-minute summary.
pub fn client_request(ip: &str, origin: &str, method: &str, path: &str) {
    let key = format!("{ip} {}", if origin.is_empty() { "(no origin)" } else { origin });
    let now = Instant::now();
    let mut c = clients().lock().unwrap_or_else(|p| p.into_inner());
    let entry = c.entry(key.clone());
    let fresh = matches!(entry, std::collections::btree_map::Entry::Vacant(_));
    let cl = entry.or_insert_with(|| Client { first: timestamp(), last: now, requests: 0, paths: BTreeMap::new() });
    let back = !fresh && now.duration_since(cl.last) > Duration::from_secs(60);
    cl.last = now;
    cl.requests += 1;
    *cl.paths.entry(format!("{method} {path}")).or_insert(0) += 1;
    drop(c);
    if fresh {
        info("http", &format!("client connected: {key} (first request {method} {path})"));
    } else if back {
        info("http", &format!("client back after a pause: {key} ({method} {path})"));
    }
}

/// One line per active client per minute: how many requests, to what.
pub fn client_summary_loop() {
    loop {
        std::thread::sleep(Duration::from_secs(60));
        let mut c = clients().lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        let mut lines = Vec::new();
        for (key, cl) in c.iter_mut() {
            if cl.paths.is_empty() {
                continue;
            }
            let top: Vec<String> = cl.paths.iter().map(|(p, n)| format!("{p} ×{n}")).collect();
            lines.push(format!("{key}: {} request(s) this minute: {}", cl.paths.values().sum::<u64>(), top.join(", ")));
            cl.paths.clear();
        }
        let gone: Vec<String> = c.iter().filter(|(_, cl)| now.duration_since(cl.last) > Duration::from_secs(120)).map(|(k, _)| k.clone()).collect();
        for k in &gone {
            c.remove(k);
        }
        drop(c);
        for l in lines {
            info("http", &l);
        }
        for k in gone {
            info("http", &format!("client gone quiet (2 min): {k}"));
        }
    }
}

/// Every client seen in the last two minutes (GET /api/diagnostics).
pub fn clients_json() -> Value {
    let c = clients().lock().unwrap_or_else(|p| p.into_inner());
    Value::Array(
        c.iter()
            .map(|(k, cl)| json!({"client": k, "since": cl.first, "requests": cl.requests, "idle_s": cl.last.elapsed().as_secs()}))
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// External commands
// ---------------------------------------------------------------------------

/// A command that could not start or that failed: logged with what the tester needs.
pub fn command_failed(args: &[&str], code: i32, stderr: &str) {
    let prog = args.first().copied().unwrap_or("?");
    let cmd = args.join(" ");
    let err = stderr.trim();
    if code == -1 && (err.contains("No such file") || err.contains("not found") || err.contains("cannot find")) {
        log_limited(
            Level::Error,
            "cmd",
            &format!("missing:{prog}"),
            Duration::from_secs(600),
            &format!("'{prog}' is not installed or not on PATH ({err}); every feature that needs it is off. First call: {cmd}"),
        );
        return;
    }
    log_limited(
        Level::Warn,
        "cmd",
        &format!("fail:{prog}:{code}:{}", args.get(1).copied().unwrap_or("")),
        Duration::from_secs(60),
        &format!("`{cmd}` exited {code}{}", if err.is_empty() { String::new() } else { format!(": {}", err.lines().next().unwrap_or("")) }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_is_iso_utc() {
        let t = timestamp();
        assert_eq!(t.len(), 24, "{t}");
        assert!(t.ends_with('Z') && t.as_bytes()[10] == b'T');
    }

    #[test]
    fn lines_reach_the_tail_and_repeats_are_limited() {
        std::env::set_var("WBS_LOG_FILE", std::env::temp_dir().join("wbs-tap-test.log"));
        let (seq0, _) = tail_since(u64::MAX, 0);
        warn("test", "first problem");
        log_limited(Level::Warn, "test", "k", Duration::from_secs(60), "limited once");
        log_limited(Level::Warn, "test", "k", Duration::from_secs(60), "limited twice");
        let (_, lines) = tail_since(seq0, 100);
        assert!(lines.iter().any(|l| l.contains("[test] first problem")));
        assert_eq!(lines.iter().filter(|l| l.contains("limited")).count(), 1);
        assert!(recent_problems(10).iter().any(|l| l.contains("first problem")));
    }
}
