// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.com - Ward-Beck.ca). All rights reserved.
//! NTP plugs in /api/ntp, offers its status to the others and keeps the log of answers.

use serde_json::Value;
use wbs_plugin::{json_reply, services, Plugin, RouteResult};

struct Ntp;

impl Plugin for Ntp {
    fn name(&self) -> &'static str {
        "ntp"
    }
    fn start(&self) {
        std::thread::spawn(crate::receipts_loop);
    }
    fn get(&self, path: &str, _query: &str) -> Option<RouteResult> {
        (path == "/api/ntp").then(|| Ok(json_reply(&crate::report())))
    }
}

impl services::Ntp for Ntp {
    fn status(&self) -> Value {
        crate::status()
    }
}

/// Plug NTP into the host.
pub fn plug() {
    wbs_plugin::register(&Ntp);
    services::provide_ntp(Box::new(Ntp));
}
