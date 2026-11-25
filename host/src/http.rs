// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.com - Ward-Beck.ca). All rights reserved.
//! Port of WBSHandler in ward-beck-audio-tab.py: the HTTP API and UI on :8467.
//!
//! The host owns only the transport: the front page, the SSE meter stream and shutdown. Every
//! other route belongs to a plugged-in application (wbs_plugin::route_get / route_post).
//!
//! One thread per request (the SSE meter stream is long-lived). JSON bodies are written
//! with Python's json.dumps formatting. A route that would have raised in the Python
//! (bad JSON body, int("abc"), ...) answers 500 with no body instead of dropping the socket.

use serde_json::{json, Map, Value};
use std::io::{Read, Write};
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, Server};

use wbs_common::{current_tai_ns, is_shutting_down, now_f64, port, shutdown_flag};
use wbs_plugin::services;
use wbs_pyshim as py;

pub use wbs_plugin::{Reply, RouteResult};

/// The TAP front page (Python HTML_UI).
pub const TAP_HTML: &str = include_str!("../web/tap.html");

/// Headers every response gets (Python WBSHandler.end_headers).
pub const CORS_HEADERS: [(&str, &str); 4] = [
    ("Access-Control-Allow-Origin", "*"),
    ("Access-Control-Allow-Methods", "GET, POST, OPTIONS"),
    ("Access-Control-Allow-Headers", "Content-Type, Cache-Control, Access-Control-Request-Private-Network, Authorization"),
    ("Access-Control-Allow-Private-Network", "true"),
];

/// Bind WBS_BIND:PORT (default 127.0.0.1, loopback only). A container on a bridge
/// network sets WBS_BIND=0.0.0.0 and publishes the port as 127.0.0.1:8467:8467.
pub fn bind() -> Result<Server, String> {
    let host = std::env::var("WBS_BIND").ok().filter(|h| !h.trim().is_empty()).unwrap_or_else(|| "127.0.0.1".into());
    Server::http((host.trim(), port())).map_err(|e| e.to_string())
}

/// Serve forever, one thread per request.
pub fn serve(server: Server) {
    for rq in server.incoming_requests() {
        std::thread::spawn(move || handle(rq));
    }
}

/// Split a request target into (path, query) like urllib.parse.urlparse.
pub fn split_target(target: &str) -> (String, String) {
    let mut t = target;
    for scheme in ["http://", "https://"] {
        if let Some(rest) = t.strip_prefix(scheme) {
            t = rest.find('/').map(|i| &rest[i..]).unwrap_or("");
        }
    }
    let t = t.split('#').next().unwrap_or("");
    match t.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (t.to_string(), String::new()),
    }
}

fn add_header(resp: &mut Response<std::io::Cursor<Vec<u8>>>, k: &str, v: &str) {
    if let Ok(h) = Header::from_bytes(k.as_bytes(), v.as_bytes()) {
        resp.add_header(h);
    }
}

fn send(rq: Request, reply: Reply) {
    let (status, mut headers, body): (u16, Vec<(String, String)>, Vec<u8>) = match reply {
        Reply::Empty(s) => (s, vec![], Vec::new()),
        Reply::Json(s, b) => (s, vec![("Content-Type".into(), "application/json".into())], b),
        Reply::Html(h) => (200, vec![("Content-Type".into(), "text/html; charset=utf-8".into())], h.as_bytes().to_vec()),
        Reply::Raw(s, h, b) => (s, h, b),
    };
    headers.extend(CORS_HEADERS.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let mut resp = Response::from_data(body).with_status_code(status);
    for (k, v) in &headers {
        add_header(&mut resp, k, v);
    }
    let _ = rq.respond(resp);
}

/// Python BaseHTTPRequestHandler.send_error(501) page for methods without a do_* handler.
fn unsupported(method: &str) -> Reply {
    let body = format!(
        "<!DOCTYPE HTML>\n<html lang=\"en\">\n    <head>\n        <meta charset=\"utf-8\">\n        \
         <title>Error response</title>\n    </head>\n    <body>\n        <h1>Error response</h1>\n        \
         <p>Error code: 501</p>\n        <p>Message: Unsupported method ('{method}').</p>\n        \
         <p>Error code explanation: 501 - Server does not support this operation.</p>\n    </body>\n</html>\n"
    );
    let body = if method == "HEAD" { Vec::new() } else { body.into_bytes() };
    Reply::Raw(501, vec![("Content-Type".into(), "text/html;charset=utf-8".into()), ("Connection".into(), "close".into())], body)
}

fn handle(mut rq: Request) {
    let (path, query) = split_target(rq.url());
    let method = rq.method().clone();
    let ip = rq.remote_addr().map(|a| a.ip().to_string()).unwrap_or_default();
    let origin = rq.headers().iter().find(|h| h.field.equiv("Origin")).map(|h| h.value.as_str().to_string()).unwrap_or_default();
    wbs_log::client_request(&ip, &origin, method.as_str(), &path);
    if path == "/api/heartbeat" || rq.headers().iter().any(|h| h.field.equiv("Sec-Fetch-Mode")) {
        services::patchbay().client_checkin();
    }
    let result = match method {
        Method::Options => Ok(Reply::Empty(204)),
        Method::Get => {
            if path == "/api/stream/meters" {
                return stream_meters(rq);
            }
            do_get(&path, &query)
        }
        Method::Post => {
            if path == "/api/abandon" {
                send(rq, Reply::Empty(200));
                shutdown_flag().store(true, std::sync::atomic::Ordering::SeqCst);
                std::thread::spawn(shutdown_sequence);
                return;
            }
            match read_body(&mut rq) {
                Ok(body) => {
                    // Every POST changes something: always logged, with who asked.
                    wbs_log::info("http", &format!("POST {path} from {ip} {origin} {}", body.chars().take(160).collect::<String>()));
                    do_post(&path, &body)
                }
                Err(e) => Err(e),
            }
        }
        ref m => Ok(unsupported(m.as_str())),
    };
    match result {
        Ok(reply) => send(rq, reply),
        Err(e) => {
            wbs_log::error("http", &format!("{} {} from {ip} failed: {e}", method.as_str(), path));
            send(rq, Reply::Empty(500));
        }
    }
}

/// Python: length = int(Content-Length or 0); body = rfile.read(length).decode() if length else "{}".
fn read_body(rq: &mut Request) -> Result<String, String> {
    let cl = rq.headers().iter().find(|h| h.field.equiv("Content-Length")).map(|h| h.value.as_str().to_string());
    let length = match cl {
        Some(v) => py::parse_int(&v)?,
        None => 0,
    };
    if length <= 0 {
        return Ok("{}".into());
    }
    let mut buf = Vec::new();
    rq.as_reader().take(length as u64).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    String::from_utf8(buf).map_err(|e| e.to_string())
}

/// The 60 FPS Server-Sent Events (SSE) live metering stream.
fn stream_meters(rq: Request) {
    let mut w = rq.into_writer();
    let mut head =
        String::from("HTTP/1.0 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n");
    for (k, v) in CORS_HEADERS {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    if w.write_all(head.as_bytes()).and_then(|_| w.flush()).is_err() {
        return;
    }
    while !is_shutting_down() {
        let (meters, (head_pos, seq), tai_ns, ntp) = {
            let _g = wbs_common::state_lock();
            (services::patchbay().meter_levels(), services::mxl().master_head(), current_tai_ns(), now_f64())
        };
        let mut m = Map::new();
        for (k, d) in [("left_db", -60.0), ("right_db", -60.0), ("left_vu", -42.0), ("right_vu", -42.0)] {
            m.insert(k.into(), meters.get(k).cloned().unwrap_or(json!(d)));
        }
        m.insert("mxl_write_head".into(), json!(head_pos));
        m.insert("mxl_write_seq".into(), json!(seq));
        m.insert("tai_ns".into(), json!(tai_ns));
        m.insert("ntp_sec".into(), json!(ntp));
        m.insert("second_of_10".into(), json!(py::round(ntp % 10.0, 4)));
        m.insert("selected_source".into(), json!(services::patchbay().selected_source()));
        let line = format!("data: {}\n\n", py::dumps(&Value::Object(m)));
        if w.write_all(line.as_bytes()).and_then(|_| w.flush()).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(16)); // ~60 FPS
    }
}

fn shutdown_sequence() {
    std::thread::sleep(Duration::from_millis(150));
    services::patchbay().abandon_ship();
    std::process::exit(0);
}

fn do_get(path: &str, query: &str) -> RouteResult {
    if path == "/" {
        return Ok(Reply::Html(TAP_HTML));
    }
    wbs_plugin::route_get(path, query).unwrap_or(Ok(Reply::Empty(404)))
}

fn do_post(path: &str, body: &str) -> RouteResult {
    wbs_plugin::route_post(path, body).unwrap_or(Ok(Reply::Empty(404)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(split_target("/api/mxl/rings?bins=60"), ("/api/mxl/rings".into(), "bins=60".into()));
        assert_eq!(split_target("/"), ("/".into(), "".into()));
        assert_eq!(split_target("http://h:1/x?y=1"), ("/x".into(), "y=1".into()));
    }

    #[test]
    fn html_checks() {
        assert!(TAP_HTML.len() > 500);
        assert!(TAP_HTML.contains("api/stream/meters"));
    }
}
