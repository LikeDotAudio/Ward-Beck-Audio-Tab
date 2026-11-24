// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! The hardware grid plugs in /api/hw and offers its links to the patchbay.

use serde_json::{json, Value};
use wbs_common::state_lock;
use wbs_plugin::reply::loads;
use wbs_plugin::{json_reply, services, Plugin, Reply, RouteResult};
use wbs_pyshim as py;

struct Hardware;

impl Plugin for Hardware {
    fn name(&self) -> &'static str {
        "hardware"
    }
    fn get(&self, path: &str, _query: &str) -> Option<RouteResult> {
        (path == "/api/hw").then(|| Ok(json_reply(&crate::status())))
    }
    fn post(&self, path: &str, body: &str) -> Option<RouteResult> {
        if path != "/api/hw" {
            return None;
        }
        // Crosspoint: {"source": "...", "sink": "...", "on": true|false}
        Some(loads(body).map(|data| {
            let g = |k: &str| data.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let on = data.get("on").map(py::truthy).unwrap_or(true);
            let out = {
                let _g = state_lock();
                crate::set(&g("source"), &g("sink"), on)
            };
            match out {
                Ok(v) => json_reply(&v),
                Err(e) => Reply::Json(400, py::dumps(&json!({"error": e})).into_bytes()),
            }
        }))
    }
}

impl services::Hardware for Hardware {
    fn set(&self, source: &str, sink: &str, on: bool) -> Result<Value, String> {
        crate::set(source, sink, on)
    }
    fn has_link_from(&self, source: &str) -> bool {
        let mods = wbs_common::pactl::run_cmd(&["pactl", "list", "short", "modules"]).1;
        crate::loopbacks(&mods).iter().any(|(_, src, ..)| src == source)
    }
}

/// Plug the hardware grid into the host.
pub fn plug() {
    wbs_plugin::register(&Hardware);
    services::provide_hardware(Box::new(Hardware));
}
