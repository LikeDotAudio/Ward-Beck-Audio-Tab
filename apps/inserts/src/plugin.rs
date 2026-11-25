// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.com - Ward-Beck.ca). All rights reserved.
//! The inserts plug in /api/inserts and offer the receivers their processing.

use wbs_plugin::reply::loads_obj;
use wbs_plugin::{json_reply, services, Plugin, RouteResult};

struct InsertsPlugin;

impl Plugin for InsertsPlugin {
    fn name(&self) -> &'static str {
        "inserts"
    }

    fn get(&self, path: &str, _query: &str) -> Option<RouteResult> {
        (path == "/api/inserts").then(|| Ok(json_reply(&crate::status())))
    }

    fn post(&self, path: &str, body: &str) -> Option<RouteResult> {
        (path == "/api/inserts").then(|| loads_obj(body).and_then(|d| crate::post(&d)).map(|v| json_reply(&v)))
    }
}

struct Service;

impl services::Inserts for Service {
    fn active(&self, slot: &str) -> bool {
        crate::active(slot)
    }
    fn process(&self, slot: &str, sample_rate: u32, planar: &mut [Vec<f32>]) {
        crate::process(slot, sample_rate, planar);
    }
}

/// Plug the inserts into the host.
pub fn plug() {
    services::provide_inserts(Box::new(Service));
    wbs_plugin::register(&InsertsPlugin);
}
