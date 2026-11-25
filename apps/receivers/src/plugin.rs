// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! The receivers plug in /api/testpoint and /api/receiver, and offer their rings and audio
//! (the MXL engine serves them next to its own flows).

use serde_json::{json, Map, Value};
use wbs_common::PcmChunk;
use wbs_plugin::reply::loads_obj;
use wbs_plugin::{json_reply, services, Plugin, RouteResult};
use wbs_pyshim as py;

use crate::{ams81, SLOTS, TEST_POINT};

struct Receivers;

impl Plugin for Receivers {
    fn name(&self) -> &'static str {
        "receivers"
    }

    fn get(&self, path: &str, _query: &str) -> Option<RouteResult> {
        match path {
            "/api/testpoint" => Some(Ok(json_reply(&crate::status()))),
            "/api/receiver" => Some(Ok(json_reply(&crate::receivers_status()))),
            "/api/ams81" => Some(Ok(json_reply(&crate::ams81_status()))),
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
            "/api/ams81" => Some(loads_obj(body).and_then(|d| ams81_post(&d)).map(|v| json_reply(&v))),
            _ => None,
        }
    }
}

/// {slot, select: "1/2".."7/8" | "A".."D" | "LoRo"} picks what the fold-down plays;
/// {slot, preset, side: L|R, ch: 1-8, db: number | null} sets one preset gain (null = off).
/// Saved after each change; the reply is every receiver's fold-down.
fn ams81_post(data: &Map<String, Value>) -> Result<Value, String> {
    let s = |k: &str| data.get(k).map(py::str_of).unwrap_or_default();
    let slot = match crate::slot_named(&s("slot")) {
        Some(i) if !SLOTS[i].fold_flow_id.is_empty() => i,
        _ => return Err(format!("slot must be network or moip, not '{}'", s("slot"))),
    };
    {
        let mut all = ams81::settings();
        if data.contains_key("select") {
            all[slot].select = ams81::Ams81::parse_select(&s("select"))
                .ok_or_else(|| format!("select must be 1/2, 3/4, 5/6, 7/8, A-D or LoRo, not '{}'", s("select")))?;
        } else if data.contains_key("preset") {
            let ch = data.get("ch").and_then(Value::as_f64).filter(|c| c.fract() == 0.0 && *c >= 1.0);
            let ch = ch.ok_or("ch must be 1 to 8")? as usize - 1;
            let db = match data.get("db") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_f64().filter(|d| d.is_finite()).ok_or("db must be a number or null")?),
            };
            all[slot].set_gain(&s("preset"), &s("side"), ch, db)?;
        } else {
            return Err("give select, or preset + side + ch + db".into());
        }
        ams81::save(&all);
    }
    Ok(crate::ams81_status())
}

impl services::Receivers for Receivers {
    fn stop_point(&self) -> bool {
        crate::stop_point()
    }
    fn stop_receivers(&self) -> bool {
        crate::stop_receivers()
    }
    fn rings(&self, bins: usize) -> (Option<Value>, Vec<Value>) {
        let rx = (1..SLOTS.len()).filter_map(|slot| crate::ring_snapshot_in(slot, bins));
        let folds = (1..SLOTS.len()).filter_map(|slot| crate::fold_snapshot_in(slot, bins));
        let others = rx.chain(folds).collect();
        (crate::ring_snapshot(bins), others)
    }
    fn test_point_status(&self) -> Value {
        crate::status()
    }
    fn receivers_status(&self) -> Value {
        crate::receivers_status()
    }
    fn pcm(&self, flow_id: &str, since: i64, max_frames: usize) -> Option<Option<PcmChunk>> {
        crate::slot_of_flow(flow_id)
            .map(|slot| crate::pcm_since_in(slot, since, max_frames))
            .or_else(|| crate::slot_of_fold(flow_id).map(|slot| crate::fold_pcm_since_in(slot, since, max_frames)))
    }
}

/// Plug the test point and the network / MoIP receivers into the host.
pub fn plug() {
    wbs_plugin::register(&Receivers);
    services::provide_receivers(Box::new(Receivers));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(body: Value) -> Result<Value, String> {
        ams81_post(body.as_object().unwrap())
    }

    /// Only rejected requests here: an accepted one saves ams81.json beside the live log.
    #[test]
    fn ams81_post_rejects() {
        assert!(post(json!({"slot": "testpoint", "select": "A"})).unwrap_err().starts_with("slot must"));
        assert!(post(json!({"slot": "network", "select": "2/3"})).unwrap_err().starts_with("select must"));
        assert!(post(json!({"slot": "moip", "preset": "A", "side": "L", "ch": 0, "db": 0})).unwrap_err().starts_with("ch must"));
        assert!(post(json!({"slot": "moip", "preset": "A", "side": "L", "ch": 9, "db": 0})).unwrap_err().starts_with("channel must"));
        assert!(post(json!({"slot": "moip", "preset": "E", "side": "L", "ch": 1, "db": 0})).unwrap_err().starts_with("preset must"));
        assert!(post(json!({"slot": "moip", "preset": "A", "side": "X", "ch": 1, "db": 0})).unwrap_err().starts_with("side must"));
        assert!(post(json!({"slot": "moip", "preset": "A", "side": "L", "ch": 1, "db": "loud"})).unwrap_err().starts_with("db must"));
        assert!(post(json!({"slot": "moip"})).unwrap_err().starts_with("give"));
    }
}
