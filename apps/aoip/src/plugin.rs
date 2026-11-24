// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! The AoIP listener plugs in /api/aoip*, offers its snapshot and the PTP leader, and starts
//! the SAP / SIP / PTP listeners (Dante device IPs come from the NMOS service).

use serde_json::Value;
use wbs_common::{shutdown_flag, state_lock};
use wbs_plugin::{json_reply, services, Plugin, RouteResult};

struct Aoip;

impl Plugin for Aoip {
    fn name(&self) -> &'static str {
        "aoip"
    }
    fn start(&self) {
        crate::set_dante_ip_provider(Box::new(|| services::nmos().dante_ips()));
        crate::start(shutdown_flag());
    }
    fn get(&self, path: &str, _query: &str) -> Option<RouteResult> {
        match path {
            "/api/aoip" => Some((|| {
                let mut payload = {
                    let _g = state_lock();
                    services::nmos().aoip_or_inspect()
                };
                let mc = crate::snapshot();
                match payload.as_object_mut() {
                    Some(o) => {
                        o.insert("multicast".into(), mc);
                    }
                    None => return Err("dict() of a non-dict AoIP report".into()),
                }
                Ok(json_reply(&payload))
            })()),
            "/api/aoip/multicast" => Some(Ok(json_reply(&crate::snapshot()))),
            _ => None,
        }
    }
}

impl services::Aoip for Aoip {
    fn snapshot(&self) -> Value {
        crate::snapshot()
    }
    fn ensure_started(&self) {
        crate::start(shutdown_flag());
    }
    fn ptp_leader(&self) -> Option<(String, i64)> {
        crate::ptp_leader()
    }
    fn features(&self) -> Value {
        serde_json::json!({"vendors": cfg!(feature = "vendors")})
    }
}

/// Plug the AoIP listener into the host.
pub fn plug() {
    wbs_plugin::register(&Aoip);
    services::provide_aoip(Box::new(Aoip));
}
