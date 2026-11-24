// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! NMOS plugs in /api/nmos, offers what it discovered, and runs the discovery loop.

use serde_json::{json, Value};
use wbs_common::state_lock;
use wbs_plugin::{json_reply, services, Plugin, RouteResult};

struct Nmos;

impl Plugin for Nmos {
    fn name(&self) -> &'static str {
        "nmos"
    }
    fn start(&self) {
        std::thread::spawn(crate::nmos_discovery_loop);
    }
    fn get(&self, path: &str, _query: &str) -> Option<RouteResult> {
        (path == "/api/nmos").then(|| {
            let _g = state_lock();
            Ok(json_reply(&json!({"status": "ok", "count": crate::nmos_count(), "nodes": crate::nmos_nodes()})))
        })
    }
}

impl services::Nmos for Nmos {
    fn nodes(&self) -> Vec<Value> {
        crate::nmos_nodes()
    }
    fn aoip_or_inspect(&self) -> Value {
        crate::aoip_or_inspect()
    }
    fn dante_ips(&self) -> Vec<String> {
        crate::dante_ips()
    }
}

/// Plug NMOS discovery into the host.
pub fn plug() {
    wbs_plugin::register(&Nmos);
    services::provide_nmos(Box::new(Nmos));
}
