// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! What a route answers, and the request helpers every plugin's routes use
//! (query strings, path segments, JSON bodies), with the Python daemon's semantics.

use serde_json::{json, Map, Value};
use wbs_pyshim as py;

/// What a route answers.
pub enum Reply {
    /// send_response(status); end_headers() (no body)
    Empty(u16),
    /// application/json body
    Json(u16, Vec<u8>),
    /// text/html; charset=utf-8 body
    Html(&'static str),
    /// Any other body with its own headers (before the CORS headers)
    Raw(u16, Vec<(String, String)>, Vec<u8>),
}

/// A route's answer, or the error that makes the host answer 500 (and log it).
pub type RouteResult = Result<Reply, String>;

/// 200 with Python json.dumps formatting.
pub fn json_reply(v: &Value) -> Reply {
    Reply::Json(200, py::dumps(v).into_bytes())
}

/// `status` with {"error": msg}.
pub fn json_error(status: u16, msg: &str) -> Reply {
    Reply::Json(status, py::dumps(&json!({"error": msg})).into_bytes())
}

fn unquote_plus(s: &str) -> String {
    let b = s.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// urllib.parse.parse_qs(query) (keep_blank_values=False), first value per key.
pub fn qs_first(query: &str, key: &str) -> Option<String> {
    for part in query.split('&') {
        let Some((k, v)) = part.split_once('=') else { continue };
        if v.is_empty() {
            continue;
        }
        if unquote_plus(k) == key {
            return Some(unquote_plus(v));
        }
    }
    None
}

/// int(qs[key]) or `default`; Err on a value that is not an int (the Python raised).
pub fn qs_int(query: &str, key: &str, default: i64) -> Result<i64, String> {
    match qs_first(query, key) {
        Some(v) => py::parse_int(&v),
        None => Ok(default),
    }
}

/// Python path.split("/")[i] (None past the end).
pub fn seg(path: &str, i: usize) -> Option<&str> {
    path.split('/').nth(i)
}

/// json.loads(body) ("" is {}).
pub fn loads(body: &str) -> Result<Value, String> {
    if body.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(body).map_err(|e| e.to_string())
}

/// json.loads(body) that must be a dict (the Python calls data.get on it).
pub fn loads_obj(body: &str) -> Result<Map<String, Value>, String> {
    match loads(body)? {
        Value::Object(o) => Ok(o),
        v => Err(format!("'{}' object has no attribute 'get'", py::type_name(&v))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_parsing() {
        assert_eq!(qs_first("bins=60&bins=70", "bins").as_deref(), Some("60"));
        assert_eq!(qs_first("bins=&x=1", "bins"), None);
        assert_eq!(qs_first("a=b%20c+d", "a").as_deref(), Some("b c d"));
        assert_eq!(qs_first("flag", "flag"), None);
        assert_eq!(qs_int("since=-5", "since", -1).unwrap(), -5);
        assert!(qs_int("since=x", "since", -1).is_err());
    }

    #[test]
    fn segments() {
        assert_eq!(seg("/api/mxl/waveform", 4), None);
        assert_eq!(seg("/api/mxl/flow/abc/pcm", 4), Some("abc"));
    }
}
