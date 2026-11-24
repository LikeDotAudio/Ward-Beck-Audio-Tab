// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! Listen plugs in /api/listen.

use serde_json::json;
use wbs_common::state_lock;
use wbs_plugin::reply::loads;
use wbs_plugin::{json_reply, Plugin, Reply, RouteResult};
use wbs_pyshim as py;

struct Listen;

impl Plugin for Listen {
    fn name(&self) -> &'static str {
        "listen"
    }
    fn get(&self, path: &str, _query: &str) -> Option<RouteResult> {
        (path == "/api/listen").then(|| {
            let _g = state_lock();
            crate::enforce();
            Ok(json_reply(&crate::state()))
        })
    }
    fn post(&self, path: &str, body: &str) -> Option<RouteResult> {
        (path == "/api/listen").then(|| {
            let data = loads(body)?;
            let ok = {
                let _g = state_lock();
                crate::listen(&data)
            };
            Ok(Reply::Json(if ok { 200 } else { 400 }, py::dumps(&json!({"ok": ok})).into_bytes()))
        })
    }
}

/// Plug the listen picker into the host.
pub fn plug() {
    wbs_plugin::register(&Listen);
}
