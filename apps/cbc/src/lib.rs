// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! Port of the CBC / Radio-Canada MXL test-set proxy in ward-beck-audio-tab.py.
//!
//! CBC/Radio-Canada MXL hands-on stack (docker-compose.mxl-demo.yml). The site can only
//! reach this daemon (:8467, CORS + Private Network Access), so every CBC service is
//! probed here and its JSON API is proxied through /api/cbc/svc/<name>/<path>.
//!
//! GET /CBC (or /cbc) is the test set in one page (web/cbc.html, built in): run state
//! with START / STOP, patterns, output level, flow ids, and each service in a tab.
//! GET /api/cbc lists the endpoints.

// Plugged into the host: routes, services and start-up (plugin.rs).
pub mod plugin;
pub use plugin::plug;

use serde_json::{json, Map, Value};
use std::io::Read;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use wbs_pyshim as py;

/// CBC_SERVICES: (name, port, label), in the Python dict order.
pub const CBC_SERVICES: [(&str, u16, &str); 4] = [
    ("generator", 9600, "Test Generator"),
    ("player", 9602, "File Player"),
    ("webrtc", 9601, "WebRTC Receiver"),
    ("info", 9699, "MXL Info GUI"),
];

/// The test set's page (GET /CBC), driven only by the /api/cbc routes.
pub const CBC_HTML: &str = include_str!("../web/cbc.html");

/// GET /api/cbc: the endpoints and services, so the API can be found.
pub fn api_index() -> Value {
    let ep = |method: &str, path: &str, body: Value, what: &str| json!({"method": method, "path": path, "body": body, "what": what});
    json!({
        "page": "/CBC",
        "endpoints": [
            ep("GET", "/api/cbc/status", Value::Null, "online, running, patterns, level_db, flows, services (up / down)"),
            ep("POST", "/api/cbc/pipeline/start", json!({"service": "generator | player (optional)"}), "start the generator pipeline"),
            ep("POST", "/api/cbc/pipeline/stop", json!({"service": "generator | player (optional)"}), "stop it"),
            ep("POST", "/api/cbc/pattern", json!({"target": "video | audio", "pattern": "<name from status.patterns>"}), "set a test pattern"),
            ep("POST", "/api/cbc/audio_level", json!({"level_db": -20}), "output level, dBFS"),
            ep("GET", "/api/cbc/svc/<service>/<path>", Value::Null, "pass-through GET to a service API"),
            ep("POST", "/api/cbc/svc/<service>/<path>", json!({}), "pass-through POST to a service API"),
        ],
        "services": CBC_SERVICES.iter().map(|(name, port, label)| json!({"name": name, "port": port, "label": label})).collect::<Vec<_>>(),
    })
}

fn st() -> MutexGuard<'static, Map<String, Value>> {
    static STATE: OnceLock<Mutex<Map<String, Value>>> = OnceLock::new();
    STATE
        .get_or_init(|| {
            let mut m = Map::new();
            m.insert("online".into(), json!(false)); // generator API reachable
            m.insert("running".into(), json!(false)); // generator pipeline running (flows being written)
            m.insert("services".into(), json!({}));
            m.insert("video_pattern".into(), json!("100% bars"));
            m.insert("audio_pattern".into(), json!("1 kHz tone"));
            m.insert("level_db".into(), json!(-20.0));
            m.insert("resolution".into(), json!("1920x1080"));
            m.insert("framerate".into(), json!("30"));
            m.insert("patterns".into(), json!({"video": [], "audio": []}));
            m.insert("flows".into(), json!({}));
            Mutex::new(m)
        })
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// CBC_GEN_STATE.
pub fn state() -> Value {
    Value::Object(st().clone())
}

fn agent(timeout: f64) -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(Duration::from_secs_f64(timeout)).build()
}

fn read_body(resp: ureq::Response) -> Vec<u8> {
    let mut b = Vec::new();
    let _ = resp.into_reader().take(64 * 1024 * 1024).read_to_end(&mut b);
    b
}

/// str(e) of the urllib exception Python would have raised for this ureq error.
pub fn error_str(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, resp) => format!("HTTP Error {}: {}", code, resp.status_text()),
        ureq::Error::Transport(t) => {
            let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(t);
            while let Some(s) = src {
                if let Some(io) = s.downcast_ref::<std::io::Error>() {
                    if matches!(io.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) {
                        return "timed out".into();
                    }
                    if let Some(n) = io.raw_os_error() {
                        let msg = std::io::Error::from_raw_os_error(n).to_string();
                        let msg = msg.split(" (os error").next().unwrap_or(&msg).to_string();
                        return format!("<urlopen error [Errno {n}] {msg}>");
                    }
                    return format!("<urlopen error {io}>");
                }
                src = s.source();
            }
            format!("<urlopen error {t}>")
        }
    }
}

/// Plain GET; Ok(body) for a 2xx/3xx answer (urllib.urlopen semantics), Err(str(e)) otherwise.
pub fn get(url: &str, headers: &[(&str, &str)], timeout: f64) -> Result<Vec<u8>, String> {
    let mut rq = agent(timeout).get(url);
    for (k, v) in headers {
        rq = rq.set(k, v);
    }
    rq.call().map(read_body).map_err(|e| error_str(&e))
}

/// POST raw bytes as application/json; Ok(body) on success, Err(str(e)) otherwise.
pub fn post_raw(url: &str, body: &[u8], timeout: f64) -> Result<Vec<u8>, String> {
    agent(timeout)
        .post(url)
        .set("Content-Type", "application/json")
        .send_bytes(body)
        .map(read_body)
        .map_err(|e| error_str(&e))
}

/// Calls a CBC service JSON API on localhost; returns (status, body_bytes).
pub fn cbc_request(service: &str, path: &str, payload: Option<&Value>, timeout: f64) -> (u16, Vec<u8>) {
    let Some(&(_, port, _)) = CBC_SERVICES.iter().find(|s| s.0 == service) else {
        let msg = json!({"error": format!("unknown CBC service '{service}'")});
        return (404, py::dumps(&msg).into_bytes());
    };
    let url = format!("http://127.0.0.1:{}/{}", port, path.trim_start_matches('/'));
    let a = agent(timeout);
    let res = match payload {
        None => a.get(&url).set("Content-Type", "application/json").set("Accept", "application/json").call(),
        Some(p) => a
            .post(&url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json")
            .send_bytes(py::dumps(p).as_bytes()),
    };
    match res {
        Ok(r) => (r.status(), read_body(r)),
        Err(ureq::Error::Status(code, r)) => (code, read_body(r)),
        Err(e) => (502, py::dumps(&json!({"error": error_str(&e)})).into_bytes()),
    }
}

/// The CBC generator's own domain path (first non-nested entry from GET /domains).
pub fn cbc_default_domain() -> String {
    let (status, body) = cbc_request("generator", "/domains", None, 1.5);
    if let Ok(v) = serde_json::from_slice::<Value>(&body) {
        let doms: Vec<Value> = if status == 200 {
            v.get("domains").and_then(|d| d.as_array()).cloned().unwrap_or_default()
        } else {
            Vec::new()
        };
        for d in &doms {
            let p = d.get("path").and_then(|p| p.as_str()).unwrap_or("");
            if p.matches('/').count() <= 1 {
                if let Some(s) = d.get("path").and_then(|p| p.as_str()) {
                    return s.to_string();
                }
                return "/mxl-domain".into(); // Python: KeyError -> fallback
            }
        }
        if let Some(s) = doms.first().and_then(|d| d.get("path")).and_then(|p| p.as_str()) {
            return s.to_string();
        }
    }
    "/mxl-domain".into()
}

/// data.get(k, {}).get(k2, default) with Python's AttributeError on a non-dict.
fn get2(data: &Value, k: &str, k2: &str, default: Value) -> Result<Value, ()> {
    match data.get(k) {
        None => Ok(default),
        Some(Value::Object(o)) => Ok(o.get(k2).cloned().unwrap_or(default)),
        Some(_) => Err(()),
    }
}

/// Polls CBC Test Generator on port 9600 for active patterns and available presets.
pub fn poll_cbc_generator() {
    let status = get("http://127.0.0.1:9600/pipeline/status", &[("Accept", "application/json")], 1.0)
        .and_then(|b| serde_json::from_slice::<Value>(&b).map_err(|e| e.to_string()));
    let ok = match status {
        Ok(data) => {
            let mut s = st();
            s.insert("online".into(), json!(true));
            (|| -> Result<(), ()> {
                let o = data.as_object().ok_or(())?;
                s.insert("running".into(), json!(py::truthy(o.get("running").unwrap_or(&json!(false)))));
                s.insert("video_pattern".into(), get2(&data, "video", "pattern", json!("100% bars"))?);
                s.insert("audio_pattern".into(), get2(&data, "audio1", "pattern", json!("1 kHz tone"))?);
                s.insert("level_db".into(), get2(&data, "audio1", "level_db", json!(-20.0))?);
                s.insert("resolution".into(), get2(&data, "video", "resolution", json!("1920x1080"))?);
                s.insert("framerate".into(), get2(&data, "video", "framerate", json!("30"))?);
                s.insert("flows".into(), o.get("flow_uuids").cloned().unwrap_or(json!({})));
                Ok(())
            })()
            .is_ok()
        }
        Err(_) => false,
    };
    if !ok {
        let mut s = st();
        s.insert("online".into(), json!(false));
        s.insert("running".into(), json!(false));
    }

    let mut services = Map::new();
    for (name, port, label) in CBC_SERVICES {
        let up = get(&format!("http://127.0.0.1:{port}/"), &[], 0.5).is_ok();
        services.insert(
            name.into(),
            json!({"label": label, "port": port, "url": format!("http://localhost:{port}/"), "up": up}),
        );
    }
    st().insert("services".into(), Value::Object(services));

    if let Ok(b) = get("http://127.0.0.1:9600/patterns", &[("Accept", "application/json")], 1.0) {
        if let Ok(p) = serde_json::from_slice::<Value>(&b) {
            st().insert("patterns".into(), p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_service() {
        let (s, b) = cbc_request("nope", "/x", None, 0.1);
        assert_eq!(s, 404);
        assert_eq!(String::from_utf8(b).unwrap(), r#"{"error": "unknown CBC service 'nope'"}"#);
    }

    #[test]
    fn initial_state_order() {
        let keys: Vec<String> = state().as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            keys,
            ["online", "running", "services", "video_pattern", "audio_pattern", "level_db", "resolution", "framerate", "patterns", "flows"]
        );
    }
}
