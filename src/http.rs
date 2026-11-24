// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! Port of WBSHandler in ward-beck-audio-tab.py: the HTTP API and UI on :8467.
//!
//! One thread per request (the SSE meter stream is long-lived). JSON bodies are written
//! with Python's json.dumps formatting. A route that would have raised in the Python
//! (bad JSON body, int("abc"), ...) answers 500 with no body instead of dropping the socket.

use serde_json::{json, Map, Value};
use std::io::{Read, Write};
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, Server};

use crate::mxl::{current_tai_ns, engine};
use crate::tap::{self, py, state_lock};
use crate::util::{is_shutting_down, now_f64, port, shutdown_flag};
use crate::{aoip, cbc, listen, nmos, pipeline, testpoint};

/// The TAP front page (Python HTML_UI).
pub const TAP_HTML: &str = include_str!("../web/tap.html");

/// Headers every response gets (Python WBSHandler.end_headers).
pub const CORS_HEADERS: [(&str, &str); 4] = [
    ("Access-Control-Allow-Origin", "*"),
    ("Access-Control-Allow-Methods", "GET, POST, OPTIONS"),
    (
        "Access-Control-Allow-Headers",
        "Content-Type, Cache-Control, Access-Control-Request-Private-Network, Authorization",
    ),
    ("Access-Control-Allow-Private-Network", "true"),
];

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

fn json_reply(v: &Value) -> Reply {
    Reply::Json(200, py::dumps(v).into_bytes())
}

type RouteResult = Result<Reply, String>;

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

fn qs_int(query: &str, key: &str, default: i64) -> Result<i64, String> {
    match qs_first(query, key) {
        Some(v) => py::parse_int(&v),
        None => Ok(default),
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
    Reply::Raw(
        501,
        vec![
            ("Content-Type".into(), "text/html;charset=utf-8".into()),
            ("Connection".into(), "close".into()),
        ],
        body,
    )
}

fn handle(mut rq: Request) {
    let (path, query) = split_target(rq.url());
    let method = rq.method().clone();
    if path == "/api/heartbeat" || rq.headers().iter().any(|h| h.field.equiv("Sec-Fetch-Mode")) {
        tap::client_checkin();
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
                Ok(body) => do_post(&path, &body),
                Err(e) => Err(e),
            }
        }
        ref m => Ok(unsupported(m.as_str())),
    };
    match result {
        Ok(reply) => send(rq, reply),
        Err(e) => {
            eprintln!("[WARD BECK TAP] {} {} failed: {}", method.as_str(), path, e);
            send(rq, Reply::Empty(500));
        }
    }
}

/// Python: length = int(Content-Length or 0); body = rfile.read(length).decode() if length else "{}".
fn read_body(rq: &mut Request) -> Result<String, String> {
    let cl = rq
        .headers()
        .iter()
        .find(|h| h.field.equiv("Content-Length"))
        .map(|h| h.value.as_str().to_string());
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

fn loads(body: &str) -> Result<Value, String> {
    if body.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(body).map_err(|e| e.to_string())
}

/// json.loads(body) that must be a dict (the Python calls data.get on it).
fn loads_obj(body: &str) -> Result<Map<String, Value>, String> {
    match loads(body)? {
        Value::Object(o) => Ok(o),
        v => Err(format!("'{}' object has no attribute 'get'", py::type_name(&v))),
    }
}

/// The 60 FPS Server-Sent Events (SSE) live metering stream.
fn stream_meters(rq: Request) {
    let mut w = rq.into_writer();
    let mut head = String::from(
        "HTTP/1.0 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n",
    );
    for (k, v) in CORS_HEADERS {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    if w.write_all(head.as_bytes()).and_then(|_| w.flush()).is_err() {
        return;
    }
    while !is_shutting_down() {
        let (meters, head_pos, seq, tai_ns, ntp) = {
            let _g = state_lock();
            let mf = engine().master_flow();
            (tap::meter_levels_json(), mf.write_head(), mf.write_seq(), current_tai_ns(), now_f64())
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
        m.insert("selected_source".into(), json!(tap::selected_source()));
        let line = format!("data: {}\n\n", py::dumps(&Value::Object(m)));
        if w.write_all(line.as_bytes()).and_then(|_| w.flush()).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(16)); // ~60 FPS
    }
}

fn shutdown_sequence() {
    std::thread::sleep(Duration::from_millis(150));
    {
        let _g = state_lock();
        tap::abandon_ship();
    }
    std::process::exit(0);
}

fn status_payload() -> Value {
    let pipeline_data = tap::get_cached_pipeline();
    let _g = state_lock();
    let eng = engine();
    let mut m = Map::new();
    m.insert("physical_sink".into(), json!(tap::physical_sink()));
    m.insert("selected_source".into(), json!(tap::selected_source()));
    m.insert("sources".into(), Value::Array(tap::available_sources()));
    m.insert("routes".into(), tap::routes_json());
    m.insert("meters".into(), tap::meter_levels_json());
    m.insert("source_meters".into(), tap::source_meters());
    m.insert("test_tone".into(), tap::test_tone_json());
    m.insert("bit_buddy".into(), tap::bitbuddy_json());
    m.insert("nmos".into(), json!({"count": nmos::nmos_count(), "nodes": nmos::nmos_nodes()}));
    m.insert("aoip".into(), nmos::aoip_or_inspect());
    m.insert("cbc_generator".into(), cbc::state());
    m.insert("all_domains".into(), eng.scan_all_domains());
    m.insert("mxl".into(), eng.get_telemetry());
    m.insert("pipeline".into(), pipeline_data);
    Value::Object(m)
}

/// Python path.split("/")[i] (None past the end).
fn seg(path: &str, i: usize) -> Option<&str> {
    path.split('/').nth(i)
}

fn do_get(path: &str, query: &str) -> RouteResult {
    Ok(match path {
        "/" => Reply::Html(TAP_HTML),
        "/api/listen" => {
            let _g = state_lock();
            listen::enforce();
            json_reply(&listen::state())
        }
        "/setup" | "/setup/" => Reply::Html(crate::setup::SETUP_HTML),
        "/api/hw" => json_reply(&crate::hwout::status()),
        "/api/status" => json_reply(&status_payload()),
        "/api/aoip" => {
            let mut payload = {
                let _g = state_lock();
                nmos::aoip_or_inspect()
            };
            let mc = aoip::snapshot();
            match payload.as_object_mut() {
                Some(o) => {
                    o.insert("multicast".into(), mc);
                }
                None => return Err("dict() of a non-dict AoIP report".into()),
            }
            json_reply(&payload)
        }
        "/api/aoip/multicast" => json_reply(&aoip::snapshot()),
        "/api/aes67" => json_reply(&crate::aes67::status()),
        "/api/nmos" => {
            let _g = state_lock();
            json_reply(&json!({"status": "ok", "count": nmos::nmos_count(), "nodes": nmos::nmos_nodes()}))
        }
        "/api/pipeline" => json_reply(&pipeline::inspect_full_pipeline(Some(engine()))),
        "/api/pipeline/alsa" => json_reply(&pipeline::inspect_alsa()),
        "/api/pipeline/pipewire" => json_reply(&pipeline::inspect_pipewire()),
        "/api/mxl" => json_reply(&engine().get_telemetry()),
        "/api/mxl/rings" => {
            // Real ring-buffer state of every flow: write head, meter read head,
            // per-channel min/max bins and peak/RMS meters
            let bins = qs_int(query, "bins", 360)?.clamp(60, 720) as usize;
            let mut rep = engine().get_rings(bins);
            let tp = testpoint::ring_snapshot(bins);
            if let Some(o) = rep.as_object_mut() {
                if let Some(tp) = tp.filter(py::truthy) {
                    let flows = o.entry("flows").or_insert_with(|| json!([]));
                    match flows.as_array_mut() {
                        Some(a) => a.insert(0, tp),
                        None => return Err("'flows' is not a list".into()),
                    }
                }
                o.insert("test_point".into(), testpoint::status());
            }
            json_reply(&rep)
        }
        "/api/testpoint" => json_reply(&testpoint::status()),
        "/api/cbc/status" => json_reply(&cbc::state()),
        "/api/bitbuddy" => {
            let _g = state_lock();
            json_reply(&tap::bitbuddy_json())
        }
        p if p == "/api/mxl/waveform" || (p.starts_with("/api/mxl/flow/") && p.contains("/waveform")) => {
            let flow_id = match seg(p, 4) {
                Some(s) if s != "waveform" => s.to_string(),
                _ => "wbs-flow-master-stereo".to_string(),
            };
            let eng = engine();
            let wf = match eng.flow(&flow_id) {
                Some(f) => f.get_waveform_bins(),
                None => eng.get_external_flow_waveform(&flow_id),
            };
            json_reply(&wf)
        }
        p if p.starts_with("/api/cbc/svc/") => {
            // GET passthrough to a CBC service API: /api/cbc/svc/<generator|player|webrtc|info>/<path>
            let rest = &p["/api/cbc/svc/".len()..];
            let (svc_name, svc_path) = rest.split_once('/').unwrap_or((rest, ""));
            let mut svc_path = svc_path.to_string();
            if !query.is_empty() {
                svc_path.push('?');
                svc_path.push_str(query);
            }
            let (status, body) = cbc::cbc_request(svc_name, &svc_path, None, 3.0);
            Reply::Json(status, body)
        }
        p if p.starts_with("/api/mxl/flow/") && p.ends_with("/pcm") => {
            // /api/mxl/flow/<flow_id>/pcm?since=<cursor>&max=<frames> -> planar Float32 LE
            let flow_id = seg(p, 4).unwrap_or("");
            let since = qs_int(query, "since", -1)?;
            let max_frames = qs_int(query, "max", 9600)?.max(0) as usize;
            let res = if flow_id == testpoint::FLOW_ID {
                testpoint::pcm_since(since, max_frames)
            } else {
                engine().get_flow_pcm(flow_id, since, max_frames)
            };
            match res {
                None => Reply::Empty(404),
                Some(c) => Reply::Raw(
                    200,
                    vec![
                        ("Content-Type".into(), "application/octet-stream".into()),
                        ("Cache-Control".into(), "no-store".into()),
                        ("X-Cursor".into(), c.cursor.to_string()),
                        ("X-Channels".into(), c.channels.to_string()),
                        ("X-Sample-Rate".into(), c.sample_rate.to_string()),
                        ("X-Frames".into(), c.frames.to_string()),
                        (
                            "Access-Control-Expose-Headers".into(),
                            "X-Cursor, X-Channels, X-Sample-Rate, X-Frames".into(),
                        ),
                    ],
                    c.payload,
                ),
            }
        }
        p if p.starts_with("/api/mxl/flow/") && p.ends_with("/levels") => {
            json_reply(&engine().get_flow_levels(seg(p, 4).unwrap_or("")))
        }
        p if p.starts_with("/api/mxl/flow/") && p.contains("/slice") => {
            // Query slice: /api/mxl/flow/<flow_id>/slice?tail=479900&count=256
            let flow_id = seg(p, 4).unwrap_or("wbs-flow-master-stereo");
            let tail = qs_int(query, "tail", 479900)?;
            let count = qs_int(query, "count", 256)?.max(0) as usize;
            let eng = engine();
            let flow = eng.flow(flow_id).unwrap_or_else(|| eng.master_flow());
            let slice = flow.get_samples(tail, count);
            let Some(o) = slice.as_object() else { return Err("slice is not a dict".into()) };
            let preview = |k: &str| -> Result<Value, String> {
                match o.get(k) {
                    Some(Value::Array(a)) => Ok(Value::Array(a.iter().take(8).cloned().collect())),
                    _ => Err(format!("KeyError: '{k}'")),
                }
            };
            // Truncate samples in JSON preview to first 8 for readability
            let p0 = preview("samples_ch0")?;
            let p1 = preview("samples_ch1")?;
            let mut out = Map::new();
            for (k, v) in o {
                if k != "samples_ch0" && k != "samples_ch1" && k != "samples_ch0_preview" && k != "samples_ch1_preview" {
                    out.insert(k.clone(), v.clone());
                }
            }
            out.insert("samples_ch0_preview".into(), p0);
            out.insert("samples_ch1_preview".into(), p1);
            json_reply(&Value::Object(out))
        }
        _ => Reply::Empty(404),
    })
}

fn do_post(path: &str, body: &str) -> RouteResult {
    Ok(match path {
        "/api/testpoint" => {
            // {kind: hw|pw|mxl|aoip, ref, label} clips the test point on; {stop: true} lifts it
            let data = loads_obj(body)?;
            let payload = if data.get("stop").map(py::truthy).unwrap_or(false) {
                testpoint::stop_point();
                testpoint::status()
            } else {
                let s = |k: &str| data.get(k).map(py::str_of).unwrap_or_default();
                json!({"active": true, "point": testpoint::set_point(&s("kind"), &s("ref"), &s("label"))})
            };
            json_reply(&payload)
        }
        "/api/hw" => {
            // Crosspoint: {"source": "...", "sink": "...", "on": true|false}
            let data = loads(body)?;
            let g = |k: &str| data.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let on = data.get("on").map(py::truthy).unwrap_or(true);
            let out = {
                let _g = state_lock();
                crate::hwout::set(&g("source"), &g("sink"), on)
            };
            match out {
                Ok(v) => json_reply(&v),
                Err(e) => Reply::Json(400, py::dumps(&json!({"error": e})).into_bytes()),
            }
        }
        "/api/listen" => {
            let data = loads(body)?;
            let ok = {
                let _g = state_lock();
                listen::listen(&data)
            };
            Reply::Json(if ok { 200 } else { 400 }, py::dumps(&json!({"ok": ok})).into_bytes())
        }
        "/api/set_source" => {
            let data = loads_obj(body)?;
            let new_src = data.get("source").cloned().unwrap_or(json!(""));
            let _g = state_lock();
            if let Some(s) = new_src.as_str() {
                let valid = tap::available_sources().iter().any(|x| x.get("name").and_then(|n| n.as_str()) == Some(s));
                if valid {
                    tap::set_selected_source(s);
                    tap::apply_routes();
                }
            }
            Reply::Empty(200)
        }
        "/api/refresh_sources" => {
            let _g = state_lock();
            tap::refresh_sources();
            Reply::Empty(200)
        }
        "/api/toggle" => {
            let data = loads_obj(body)?;
            let route = data.get("route").cloned().unwrap_or(Value::Null);
            let _g = state_lock();
            tap::toggle_route(&route);
            Reply::Empty(200)
        }
        "/api/heartbeat" => json_reply(&json!({"idle_limit_s": tap::client_timeout_secs()})),
        "/api/toggle_test_tone" | "/api/test_tone" => {
            let data = loads_obj(body)?;
            let freq = data.get("freq").cloned().unwrap_or(json!(1000));
            let restart = data.get("restart");
            {
                let _g = state_lock();
                match restart {
                    Some(Value::Bool(true)) => tap::start_test_tone(&freq)?,
                    Some(Value::Bool(false)) => tap::stop_test_tone(),
                    _ if tap::test_tone_active() => tap::stop_test_tone(),
                    _ => tap::start_test_tone(&freq)?,
                }
            }
            json_reply(&tap::test_tone_json())
        }
        "/api/bitbuddy" => {
            let data = loads_obj(body)?;
            let cfg = tap::bitbuddy_config();
            let pick = |k: &str, d: Value| data.get(k).cloned().or_else(|| cfg.get(k).cloned()).unwrap_or(d);
            let pattern = pick("pattern", json!("prbs"));
            let pro_mode = pick("pro_mode", json!(true));
            let audio_data = pick("audio_data", json!(true));
            let sample_rate = pick("sample_rate", json!(48000));
            {
                let _g = state_lock();
                let start = || tap::start_bit_buddy(&pattern, &pro_mode, &audio_data, &sample_rate);
                match data.get("active") {
                    Some(Value::Bool(false)) => tap::stop_bit_buddy(),
                    Some(Value::Bool(true)) => start()?,
                    _ if tap::bitbuddy_active() => {
                        if data.get("restart").map(py::truthy).unwrap_or(false) {
                            start()?
                        } else {
                            tap::stop_bit_buddy()
                        }
                    }
                    _ => start()?,
                }
            }
            json_reply(&tap::bitbuddy_json())
        }
        "/api/force_speakers" => {
            {
                let _g = state_lock();
                tap::force_speakers();
            }
            json_reply(&json!({"status": "ok", "speaker_split": true, "physical_sink": tap::physical_sink()}))
        }
        "/api/cbc/pattern" => {
            let data = loads_obj(body)?;
            let target = data.get("target").cloned().unwrap_or(json!("video"));
            let pattern = data.get("pattern").cloned().unwrap_or(json!(""));
            let url = if target == json!("video") {
                "http://127.0.0.1:9600/video/test-pattern"
            } else {
                "http://127.0.0.1:9600/audio/flow1/test-pattern"
            };
            cbc_post_reply(url, &json!({"pattern": pattern}))
        }
        "/api/cbc/pipeline/start" | "/api/cbc/pipeline/stop" => {
            // Start/stop the CBC test generator pipeline (or the file player with {"service":"player"})
            let mut data = match loads(body) {
                Ok(Value::Object(o)) => o,
                Ok(v) => return Err(format!("'{}' object has no attribute 'pop'", py::type_name(&v))),
                Err(_) => Map::new(),
            };
            let service = data.shift_remove("service").unwrap_or(json!("generator"));
            let service = py::str_of(&service);
            let action = path.rsplit('/').next().unwrap_or("");
            if action == "start" {
                if !data.contains_key("domain") {
                    data.insert("domain".into(), json!(cbc::cbc_default_domain()));
                }
                if service == "generator" {
                    // StartRequest requires domain + all four flow configs; fill CBC defaults
                    for (key, label) in [
                        ("video", "Test-Video"),
                        ("audio1", "Test-Audio-1"),
                        ("audio2", "Test-Audio-2"),
                        ("ancillary", "Test-Anc"),
                    ] {
                        if !data.contains_key(key) {
                            data.insert(
                                key.into(),
                                json!({"active": key != "ancillary", "label": label,
                                       "description": format!("CBC test generator {label}")}),
                            );
                        }
                    }
                }
            }
            let (status, resp) = cbc::cbc_request(&service, &format!("/pipeline/{action}"), Some(&Value::Object(data)), 8.0);
            cbc::poll_cbc_generator();
            Reply::Json(status, resp)
        }
        p if p.starts_with("/api/cbc/svc/") => {
            // POST passthrough to a CBC service API
            let rest = &p["/api/cbc/svc/".len()..];
            let (svc_name, svc_path) = rest.split_once('/').unwrap_or((rest, ""));
            let data = loads(body).unwrap_or_else(|_| json!({}));
            let (status, resp) = cbc::cbc_request(svc_name, svc_path, Some(&data), 3.0);
            cbc::poll_cbc_generator();
            Reply::Json(status, resp)
        }
        "/api/cbc/audio_level" => {
            let data = loads_obj(body)?;
            let level_db = py::float_of(data.get("level_db").unwrap_or(&json!(-20.0)))?;
            cbc_post_reply("http://127.0.0.1:9600/audio/flow1/level", &json!({"level_db": level_db}))
        }
        "/api/aes67" => json_reply(&crate::aes67::control(&loads_obj(body)?)?),
        "/api/reset_all" => json_reply(&tap::reset_all()),
        "/api/restart" => {
            tap::full_restart();
            Reply::Empty(200)
        }
        _ => Reply::Empty(404),
    })
}

/// POST to the CBC generator; 200 + its body, or 500 {"error": str(e)}.
fn cbc_post_reply(url: &str, payload: &Value) -> Reply {
    match cbc::post_raw(url, py::dumps(payload).as_bytes(), 2.0) {
        Ok(body) => {
            cbc::poll_cbc_generator();
            Reply::Json(200, body)
        }
        Err(e) => Reply::Json(500, py::dumps(&json!({"error": e})).into_bytes()),
    }
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

    #[test]
    fn html_checks() {
        assert!(TAP_HTML.len() > 500);
        assert!(TAP_HTML.contains("api/stream/meters"));
    }
}
