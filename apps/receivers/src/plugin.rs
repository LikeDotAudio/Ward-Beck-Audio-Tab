// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! The receivers plug in /api/testpoint and /api/receiver, and offer their rings and audio
//! (the MXL engine serves them next to its own flows).

use serde_json::{json, Value};
use wbs_common::PcmChunk;
use wbs_plugin::reply::loads_obj;
use wbs_plugin::{json_reply, services, Plugin, RouteResult};
use wbs_pyshim as py;

use crate::{SLOTS, TEST_POINT};

struct Receivers;

impl Plugin for Receivers {
    fn name(&self) -> &'static str {
        "receivers"
    }

    fn get(&self, path: &str, _query: &str) -> Option<RouteResult> {
        match path {
            "/api/testpoint" => Some(Ok(json_reply(&crate::status()))),
            "/api/receiver" => Some(Ok(json_reply(&crate::receivers_status()))),
            _ => None,
        }
    }

    fn post(&self, path: &str, body: &str) -> Option<RouteResult> {
        match path {
            "/api/testpoint" => Some((|| {
                // {kind: hw|pw|mxl|aoip, ref, label} clips the test point on; {stop: true} lifts it
                let data = loads_obj(body)?;
                let payload = if data.get("stop").map(py::truthy).unwrap_or(false) {
                    crate::stop_point();
                    crate::status()
                } else {
                    let s = |k: &str| data.get(k).map(py::str_of).unwrap_or_default();
                    json!({"active": true, "point": crate::set_point(&s("kind"), &s("ref"), &s("label"))})
                };
                Ok(json_reply(&payload))
            })()),
            "/api/receiver" => Some((|| {
                // {slot: network|moip, kind: aoip (default), ref, label} starts that receiver;
                // {slot, stop: true} stops it. The reply is every receiver's status.
                let data = loads_obj(body)?;
                let s = |k: &str| data.get(k).map(py::str_of).unwrap_or_default();
                let slot = match crate::slot_named(&s("slot")) {
                    Some(i) if i != TEST_POINT => i,
                    _ => return Err(format!("slot must be network or moip, not '{}'", s("slot"))),
                };
                if data.get("stop").map(py::truthy).unwrap_or(false) {
                    crate::stop_in(slot);
                } else {
                    let kind = if s("kind").is_empty() { "aoip".to_string() } else { s("kind") };
                    crate::set_in(slot, &kind, &s("ref"), &s("label"));
                }
                Ok(json_reply(&crate::receivers_status()))
            })()),
            _ => None,
        }
    }
}

impl services::Receivers for Receivers {
    fn stop_point(&self) -> bool {
        crate::stop_point()
    }
    fn stop_receivers(&self) -> bool {
        crate::stop_receivers()
    }
    fn rings(&self, bins: usize) -> (Option<Value>, Vec<Value>) {
        let others = (1..SLOTS.len()).filter_map(|slot| crate::ring_snapshot_in(slot, bins)).collect();
        (crate::ring_snapshot(bins), others)
    }
    fn test_point_status(&self) -> Value {
        crate::status()
    }
    fn receivers_status(&self) -> Value {
        crate::receivers_status()
    }
    fn pcm(&self, flow_id: &str, since: i64, max_frames: usize) -> Option<Option<PcmChunk>> {
        crate::slot_of_flow(flow_id).map(|slot| crate::pcm_since_in(slot, since, max_frames))
    }
}

/// Plug the test point and the network / MoIP receivers into the host.
pub fn plug() {
    wbs_plugin::register(&Receivers);
    services::provide_receivers(Box::new(Receivers));
}
