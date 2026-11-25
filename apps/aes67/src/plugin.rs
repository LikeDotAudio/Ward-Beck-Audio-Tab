// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! The AES67 transmitters plug in /api/aes67, offer their status, and start any that
//! WBS_AES67 asks for.

use serde_json::Value;
use wbs_plugin::reply::loads_obj;
use wbs_plugin::{json_reply, services, Plugin, RouteResult};

struct Aes67;

impl Plugin for Aes67 {
    fn name(&self) -> &'static str {
        "aes67"
    }
    fn start(&self) {
        crate::autostart();
    }
    fn get(&self, path: &str, _query: &str) -> Option<RouteResult> {
        (path == "/api/aes67").then(|| Ok(json_reply(&crate::status())))
    }
    fn post(&self, path: &str, body: &str) -> Option<RouteResult> {
        (path == "/api/aes67").then(|| Ok(json_reply(&crate::control(&loads_obj(body)?)?)))
    }
}

impl services::Aes67 for Aes67 {
    fn status(&self) -> Value {
        crate::status()
    }
    fn stop_all(&self) -> Vec<String> {
        crate::stop_all()
    }
    fn groups(&self) -> Vec<String> {
        crate::groups().iter().map(|g| g.to_string()).collect()
    }
}

/// Plug the AES67 transmitters into the host.
pub fn plug() {
    wbs_plugin::register(&Aes67);
    services::provide_aes67(Box::new(Aes67));
}
