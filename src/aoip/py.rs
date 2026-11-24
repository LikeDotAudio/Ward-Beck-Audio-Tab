// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! Small helpers that reproduce Python semantics the original code relied on
//! (round(), str(), truthiness, int(), str.strip()/split(), OSError text), so the
//! JSON and messages produced by aoip / testpoint are byte-identical to the Python.

use serde_json::Value;
use std::io;

/// Python round(x, n): correctly rounded, ties to even on the exact binary value.
pub fn round(x: f64, n: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{:.*}", n, x).parse().unwrap_or(x)
}

/// Python float repr (shortest round-trip, exponent outside 1e-4 .. 1e16).
pub fn float_repr(v: f64) -> String {
    if v.is_nan() {
        return "nan".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let e = format!("{:e}", v); // e.g. "-1.25e-1"
    let (mant, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let neg = mant.starts_with('-');
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    let sign = if neg { "-" } else { "" };
    if (-4..16).contains(&exp) {
        let s = if exp >= 0 {
            let int_len = exp as usize + 1;
            if digits.len() <= int_len {
                format!("{}{}.0", digits, "0".repeat(int_len - digits.len()))
            } else {
                format!("{}.{}", &digits[..int_len], &digits[int_len..])
            }
        } else {
            format!("0.{}{}", "0".repeat((-exp - 1) as usize), digits)
        };
        format!("{sign}{s}")
    } else {
        let m = if digits.len() > 1 { format!("{}.{}", &digits[..1], &digits[1..]) } else { digits };
        format!("{sign}{m}e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs())
    }
}

/// Python format(v, "g") (6 significant digits, trailing zeros stripped).
pub fn fmt_g(v: f64) -> String {
    if !v.is_finite() {
        return float_repr(v);
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    let e = format!("{:.5e}", v);
    let (_, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let strip = |s: String| -> String {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s
        }
    };
    if (-4..6).contains(&exp) {
        strip(format!("{:.*}", (5 - exp) as usize, v))
    } else {
        let (m, _) = e.split_once('e').unwrap_or((&e, ""));
        format!("{}e{}{:02}", strip(m.to_string()), if exp < 0 { '-' } else { '+' }, exp.abs())
    }
}

/// Python str(value) for JSON-ish values.
pub fn str_of(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True".into() } else { "False".into() },
        Value::Number(n) => {
            if n.is_f64() {
                float_repr(n.as_f64().unwrap_or(0.0))
            } else {
                n.to_string()
            }
        }
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Python truthiness.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python str.isspace() for one char.
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

/// Python str.strip().
pub fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// Python str.split() (no separator).
pub fn split_ws(s: &str) -> Vec<&str> {
    s.split(is_space).filter(|p| !p.is_empty()).collect()
}

/// Python str.split(None, 1).
pub fn split_ws_once(s: &str) -> Vec<&str> {
    let s = s.trim_start_matches(is_space);
    if s.is_empty() {
        return vec![];
    }
    match s.find(is_space) {
        None => vec![s],
        Some(i) => {
            let rest = s[i..].trim_start_matches(is_space);
            if rest.is_empty() { vec![&s[..i]] } else { vec![&s[..i], rest] }
        }
    }
}

/// Python str.isdigit() restricted to ASCII digits (non-empty).
pub fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Python int(str): surrounding whitespace, optional sign, digits with single '_' separators.
pub fn parse_int(s: &str) -> Option<i64> {
    let t = strip(s);
    let (neg, body) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if body.is_empty() || body.starts_with('_') || body.ends_with('_') || body.contains("__") {
        return None;
    }
    let digits: String = body.chars().filter(|&c| c != '_').collect();
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let v: i64 = digits.parse().ok()?;
    Some(if neg { -v } else { v })
}

/// Python float(str).
pub fn parse_float(s: &str) -> Option<f64> {
    let t = strip(s);
    if t.is_empty() || t.contains("__") || t.starts_with('_') || t.ends_with('_') {
        return None;
    }
    let t: String = t.chars().filter(|&c| c != '_').collect();
    let l = t.to_ascii_lowercase();
    let body = l.trim_start_matches(['+', '-']);
    if body == "inf" || body == "infinity" || body == "nan" {
        return l.parse().ok().or_else(|| {
            let neg = l.starts_with('-');
            Some(match body {
                "nan" => f64::NAN,
                _ => if neg { f64::NEG_INFINITY } else { f64::INFINITY },
            })
        });
    }
    if !t.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-')) {
        return None;
    }
    t.parse().ok()
}

/// Python int(value) for a JSON value; Err carries the Python exception text.
pub fn int_of(v: &Value) -> Result<i64, (&'static str, String)> {
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_u64().map(|u| u as i64))
            .or_else(|| n.as_f64().map(|f| f.trunc() as i64))
            .ok_or(("ValueError", "cannot convert number".into())),
        Value::Bool(b) => Ok(*b as i64),
        Value::String(s) => parse_int(s)
            .ok_or(("ValueError", format!("invalid literal for int() with base 10: '{}'", s))),
        Value::Null => Err((
            "TypeError",
            "int() argument must be a string, a bytes-like object or a real number, not 'NoneType'".into(),
        )),
        Value::Array(_) => Err((
            "TypeError",
            "int() argument must be a string, a bytes-like object or a real number, not 'list'".into(),
        )),
        Value::Object(_) => Err((
            "TypeError",
            "int() argument must be a string, a bytes-like object or a real number, not 'dict'".into(),
        )),
    }
}

/// bytes.decode("ascii", "replace").
pub fn decode_ascii(b: &[u8]) -> String {
    b.iter().map(|&c| if c < 0x80 { c as char } else { '\u{FFFD}' }).collect()
}

/// Python max(a, b) / min(a, b) (first wins unless the second compares greater / less).
pub fn max2(a: f64, b: f64) -> f64 {
    if b > a { b } else { a }
}

/// Python max(seq) over floats (NaN behaviour included). `seq` must be non-empty.
pub fn max_of(seq: &[f32]) -> f64 {
    let mut m = seq[0];
    for &v in &seq[1..] {
        if v > m {
            m = v;
        }
    }
    m as f64
}

pub fn min_of(seq: &[f32]) -> f64 {
    let mut m = seq[0];
    for &v in &seq[1..] {
        if v < m {
            m = v;
        }
    }
    m as f64
}

/// CPython 3.12 sum() of floats (Neumaier compensated summation).
pub fn fsum_neumaier<I: IntoIterator<Item = f64>>(it: I) -> f64 {
    let mut f = 0.0f64;
    let mut c = 0.0f64;
    for x in it {
        let t = f + x;
        if f.abs() >= x.abs() {
            c += (f - t) + x;
        } else {
            c += (x - t) + f;
        }
        f = t;
    }
    if c != 0.0 && c.is_finite() {
        f += c;
    }
    f
}

/// Python exception class name for an OSError with this errno.
pub fn exc_class(e: &io::Error) -> &'static str {
    #[cfg(windows)]
    return match e.kind() {
        io::ErrorKind::PermissionDenied => "PermissionError",
        io::ErrorKind::ConnectionRefused => "ConnectionRefusedError",
        io::ErrorKind::ConnectionReset => "ConnectionResetError",
        io::ErrorKind::ConnectionAborted => "ConnectionAbortedError",
        io::ErrorKind::BrokenPipe => "BrokenPipeError",
        io::ErrorKind::TimedOut => "TimeoutError",
        io::ErrorKind::NotFound => "FileNotFoundError",
        io::ErrorKind::AlreadyExists => "FileExistsError",
        io::ErrorKind::Interrupted => "InterruptedError",
        io::ErrorKind::WouldBlock => "BlockingIOError",
        _ => "OSError",
    };
    #[cfg(unix)]
    match e.raw_os_error() {
        Some(libc::EPERM) | Some(libc::EACCES) => "PermissionError",
        Some(libc::ECONNREFUSED) => "ConnectionRefusedError",
        Some(libc::ECONNRESET) => "ConnectionResetError",
        Some(libc::ECONNABORTED) => "ConnectionAbortedError",
        Some(libc::EPIPE) | Some(libc::ESHUTDOWN) => "BrokenPipeError",
        Some(libc::ETIMEDOUT) => "TimeoutError",
        Some(libc::ENOENT) => "FileNotFoundError",
        Some(libc::EEXIST) => "FileExistsError",
        Some(libc::EINTR) => "InterruptedError",
        Some(libc::EAGAIN) | Some(libc::EALREADY) | Some(libc::EINPROGRESS) => "BlockingIOError",
        Some(libc::ECHILD) => "ChildProcessError",
        Some(libc::EISDIR) => "IsADirectoryError",
        Some(libc::ENOTDIR) => "NotADirectoryError",
        Some(libc::ESRCH) => "ProcessLookupError",
        None if e.kind() == io::ErrorKind::TimedOut => "TimeoutError",
        _ => "OSError",
    }
}

/// str(OSError): "[Errno 98] Address already in use" (or "timed out" for socket timeouts).
pub fn os_error(e: &io::Error) -> String {
    match e.raw_os_error() {
        Some(n) => {
            let s = io::Error::from_raw_os_error(n).to_string();
            let msg = s.split(" (os error").next().unwrap_or(&s).to_string();
            format!("[Errno {}] {}", n, msg)
        }
        None => {
            if e.kind() == io::ErrorKind::TimedOut || e.kind() == io::ErrorKind::WouldBlock {
                return "timed out".into();
            }
            let s = e.to_string();
            if let Some(rest) = s.strip_prefix("failed to lookup address information: ") {
                let code = match rest {
                    "Temporary failure in name resolution" => -3,
                    "Name or service not known" => -2,
                    "No address associated with hostname" => -5,
                    _ => -2,
                };
                return format!("[Errno {}] {}", code, rest);
            }
            s
        }
    }
}

/// True for the errors Python reports as socket.timeout on a socket with a timeout.
pub fn is_timeout(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

/// socket.if_nameindex(): [(index, name)].
#[cfg(windows)]
pub fn if_nameindex() -> Vec<(u32, String)> {
    crate::sys::interfaces()
}

/// socket.if_nameindex(): [(index, name)].
#[cfg(unix)]
pub fn if_nameindex() -> Vec<(u32, String)> {
    let mut out = Vec::new();
    unsafe {
        let p = libc::if_nameindex();
        if p.is_null() {
            return out;
        }
        let mut q = p;
        while (*q).if_index != 0 && !(*q).if_name.is_null() {
            let name = std::ffi::CStr::from_ptr((*q).if_name).to_string_lossy().into_owned();
            out.push(((*q).if_index, name));
            q = q.add(1);
        }
        libc::if_freenameindex(p);
    }
    out
}

#[cfg(unix)]
extern "C" {
    fn inet_ntop(af: libc::c_int, src: *const libc::c_void, dst: *mut libc::c_char, size: libc::socklen_t) -> *const libc::c_char;
}

/// socket.inet_ntop for a 4- or 16-byte address.
#[cfg(windows)]
pub fn ntop(raw: &[u8]) -> String {
    match raw.len() {
        4 => std::net::Ipv4Addr::new(raw[0], raw[1], raw[2], raw[3]).to_string(),
        16 => {
            let mut b = [0u8; 16];
            b.copy_from_slice(raw);
            std::net::Ipv6Addr::from(b).to_string()
        }
        _ => String::new(),
    }
}

/// socket.inet_ntop for a 4- or 16-byte address.
#[cfg(unix)]
pub fn ntop(raw: &[u8]) -> String {
    let (af, len) = if raw.len() == 16 { (libc::AF_INET6, 16) } else { (libc::AF_INET, 4) };
    let mut buf = [0 as libc::c_char; 64];
    unsafe {
        let r = inet_ntop(af, raw.as_ptr() as *const libc::c_void, buf.as_mut_ptr(), buf.len() as _);
        if r.is_null() {
            let _ = len;
            return String::new();
        }
        std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

/// Sleep up to `secs`, returning early (true) if `stop` is set (Python Event.wait).
pub fn wait(stop: &std::sync::atomic::AtomicBool, secs: f64) -> bool {
    let end = std::time::Instant::now() + std::time::Duration::from_secs_f64(secs.max(0.0));
    loop {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return true;
        }
        let now = std::time::Instant::now();
        if now >= end {
            return false;
        }
        std::thread::sleep((end - now).min(std::time::Duration::from_millis(50)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_matches_python() {
        assert_eq!(round(0.125, 2), 0.12);
        assert_eq!(round(0.375, 2), 0.38);
        assert_eq!(round(2.675, 2), 2.67);
        assert_eq!(round(21.3333333, 3), 21.333);
        assert_eq!(round(-0.04, 1).to_string(), "-0");
    }

    #[test]
    fn reprs() {
        assert_eq!(float_repr(1.0), "1.0");
        assert_eq!(float_repr(0.125), "0.125");
        assert_eq!(float_repr(1e16), "1e+16");
        assert_eq!(float_repr(0.0001), "0.0001");
        assert_eq!(float_repr(0.00001), "1e-05");
        assert_eq!(float_repr(123456.0), "123456.0");
        assert_eq!(fmt_g(48.0), "48");
        assert_eq!(fmt_g(44.1), "44.1");
        assert_eq!(fmt_g(22.05), "22.05");
        assert_eq!(fmt_g(1234567.0), "1.23457e+06");
        assert_eq!(fmt_g(0.00001), "1e-05");
    }

    #[test]
    fn ints_and_splits() {
        assert_eq!(parse_int(" +5_004 "), Some(5004));
        assert_eq!(parse_int("5__0"), None);
        assert_eq!(parse_int("-"), None);
        assert_eq!(split_ws_once("96  L24/48000/2 "), vec!["96", "L24/48000/2 "]);
        assert_eq!(split_ws(" a  b "), vec!["a", "b"]);
        assert_eq!(parse_float(" 0.125 "), Some(0.125));
        assert_eq!(parse_float("1e0"), Some(1.0));
        assert_eq!(parse_float("x"), None);
    }

    #[test]
    fn neumaier() {
        let v = [1e100, 1.0, -1e100];
        assert_eq!(fsum_neumaier(v.iter().copied()), 1.0);
    }
}
