// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! The CBC test set plugs in its proxy routes, its page (/CBC) and its endpoint index
//! (/api/cbc), and offers its state and HTTP helpers.

use serde_json::{json, Map, Value};
use wbs_plugin::reply::{loads, loads_obj};
use wbs_plugin::{json_reply, services, Plugin, Reply, RouteResult};
use wbs_pyshim as py;

use crate::{cbc_default_domain, cbc_request, poll_cbc_generator, post_raw};

struct Cbc;

/// POST to the CBC generator; 200 + its body, or 500 {"error": str(e)}.
fn cbc_post_reply(url: &str, payload: &Value) -> Reply {
    match post_raw(url, py::dumps(payload).as_bytes(), 2.0) {
        Ok(body) => {
            poll_cbc_generator();
            Reply::Json(200, body)
        }
        Err(e) => Reply::Json(500, py::dumps(&json!({"error": e})).into_bytes()),
    }
}

impl Plugin for Cbc {
    fn name(&self) -> &'static str {
        "cbc"
    }

    fn get(&self, path: &str, query: &str) -> Option<RouteResult> {
        if is_page(path) {
            return Some(Ok(Reply::Html(crate::CBC_HTML)));
        }
        if path == "/api/cbc" || path == "/api/cbc/" {
            return Some(Ok(json_reply(&crate::api_index())));
        }
        if path == "/api/cbc/status" {
            return Some(Ok(json_reply(&crate::state())));
        }
        let rest = path.strip_prefix("/api/cbc/svc/")?;
        // GET passthrough to a CBC service API: /api/cbc/svc/<generator|player|webrtc|info>/<path>
        let (svc_name, svc_path) = rest.split_once('/').unwrap_or((rest, ""));
        let mut svc_path = svc_path.to_string();
        if !query.is_empty() {
            svc_path.push('?');
            svc_path.push_str(query);
        }
        let (status, body) = cbc_request(svc_name, &svc_path, None, 3.0);
        Some(Ok(Reply::Json(status, body)))
    }

    fn post(&self, path: &str, body: &str) -> Option<RouteResult> {
        let ours = matches!(path, "/api/cbc/pattern" | "/api/cbc/pipeline/start" | "/api/cbc/pipeline/stop" | "/api/cbc/audio_level")
            || path.starts_with("/api/cbc/svc/");
        ours.then(|| post(path, body))
    }
}

/// The test set's page: /CBC or /cbc (with or without the trailing slash).
fn is_page(path: &str) -> bool {
    matches!(path.trim_end_matches('/'), "/CBC" | "/cbc")
}

fn post(path: &str, body: &str) -> RouteResult {
    Ok(match path {
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
                    data.insert("domain".into(), json!(cbc_default_domain()));
                }
                if service == "generator" {
                    // StartRequest requires domain + all four flow configs; fill CBC defaults
                    for (key, label) in
                        [("video", "Test-Video"), ("audio1", "Test-Audio-1"), ("audio2", "Test-Audio-2"), ("ancillary", "Test-Anc")]
                    {
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
            let (status, resp) = cbc_request(&service, &format!("/pipeline/{action}"), Some(&Value::Object(data)), 8.0);
            poll_cbc_generator();
            Reply::Json(status, resp)
        }
        p if p.starts_with("/api/cbc/svc/") => {
            // POST passthrough to a CBC service API
            let rest = &p["/api/cbc/svc/".len()..];
            let (svc_name, svc_path) = rest.split_once('/').unwrap_or((rest, ""));
            let data = loads(body).unwrap_or_else(|_| json!({}));
            let (status, resp) = cbc_request(svc_name, svc_path, Some(&data), 3.0);
            poll_cbc_generator();
            Reply::Json(status, resp)
        }
        "/api/cbc/audio_level" => {
            let data = loads_obj(body)?;
            let level_db = py::float_of(data.get("level_db").unwrap_or(&json!(-20.0)))?;
            cbc_post_reply("http://127.0.0.1:9600/audio/flow1/level", &json!({"level_db": level_db}))
        }
        _ => Reply::Empty(404),
    })
}

impl services::Cbc for Cbc {
    fn state(&self) -> Value {
        crate::state()
    }
    fn poll_generator(&self) {
        poll_cbc_generator()
    }
    fn post_raw(&self, url: &str, body: &[u8], timeout: f64) -> Result<Vec<u8>, String> {
        post_raw(url, body, timeout)
    }
    fn get(&self, url: &str, headers: &[(&str, &str)], timeout: f64) -> Result<Vec<u8>, String> {
        crate::get(url, headers, timeout)
    }
}

/// Plug the CBC test set into the host.
pub fn plug() {
    wbs_plugin::register(&Cbc);
    services::provide_cbc(Box::new(Cbc));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html(path: &str) -> bool {
        matches!(Cbc.get(path, ""), Some(Ok(Reply::Html(h))) if h.contains("<title>CBC MXL Test Set</title>"))
    }

    #[test]
    fn page_routes() {
        assert!(html("/CBC"));
        assert!(html("/cbc"));
        assert!(html("/CBC/"));
        assert!(Cbc.get("/Cbc", "").is_none());
        assert!(Cbc.get("/cbcx", "").is_none());
        assert!(Cbc.post("/CBC", "").is_none());
    }

    #[test]
    fn api_index_route() {
        let Some(Ok(Reply::Json(200, body))) = Cbc.get("/api/cbc", "") else { panic!("no index") };
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["page"], "/CBC");
        let paths: Vec<&str> = v["endpoints"].as_array().unwrap().iter().map(|e| e["path"].as_str().unwrap()).collect();
        for p in ["/api/cbc/status", "/api/cbc/pattern", "/api/cbc/audio_level", "/api/cbc/pipeline/start", "/api/cbc/pipeline/stop"] {
            assert!(paths.contains(&p), "{p}");
        }
        assert_eq!(v["services"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn page_uses_only_the_api() {
        let h = crate::CBC_HTML;
        for p in ["/api/cbc/status", "/api/cbc/pattern", "/api/cbc/audio_level", "/api/cbc/pipeline/start", "/api/cbc/pipeline/stop"] {
            assert!(h.contains(p), "{p}");
        }
        assert!(!h.contains("src=\"http") && !h.contains("href=\"http"), "no external assets");
    }
}
