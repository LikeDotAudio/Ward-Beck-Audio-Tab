// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! The patchbay plugs in /api/status, the source / route / generator controls, Reset All and
//! restart; offers its buses and sources to the others; and at start-up builds the WBS
//! topology, then runs the meter reader and the idle-client watchdog.

use serde_json::{json, Map, Value};
use wbs_common::state_lock;
use wbs_plugin::reply::loads_obj;
use wbs_plugin::{json_reply, services, Plugin, Reply, RouteResult};
use wbs_pyshim as py;

use crate as tap;

struct Patchbay;

/// GET /api/status: the whole daemon at a glance (the other applications through their services).
fn status_payload() -> Value {
    let pipeline_data = tap::get_cached_pipeline();
    let _g = state_lock();
    let mxl = services::mxl();
    let nmos = services::nmos();
    let nodes = nmos.nodes();
    let mut m = Map::new();
    m.insert("physical_sink".into(), json!(tap::physical_sink()));
    m.insert("selected_source".into(), json!(tap::selected_source()));
    m.insert("sources".into(), Value::Array(tap::available_sources()));
    m.insert("routes".into(), tap::routes_json());
    m.insert("meters".into(), tap::meter_levels_json());
    m.insert("source_meters".into(), tap::source_meters());
    m.insert("test_tone".into(), tap::test_tone_json());
    m.insert("bit_buddy".into(), tap::bitbuddy_json());
    m.insert("nmos".into(), json!({"count": nodes.len(), "nodes": nodes}));
    m.insert("aoip".into(), nmos.aoip_or_inspect());
    m.insert("cbc_generator".into(), services::cbc().state());
    m.insert("all_domains".into(), mxl.scan_all_domains());
    m.insert("mxl".into(), mxl.telemetry());
    m.insert("pipeline".into(), pipeline_data);
    Value::Object(m)
}

fn post(path: &str, body: &str) -> RouteResult {
    Ok(match path {
        "/api/set_source" => {
            let data = loads_obj(body)?;
            let new_src = data.get("source").cloned().unwrap_or(json!(""));
            let _g = state_lock();
            if let Some(s) = new_src.as_str() {
                let valid = tap::available_sources().iter().any(|x| x.get("name").and_then(|n| n.as_str()) == Some(s));
                if valid {
                    tap::set_selected_source(s);
                    tap::apply_routes();
                }
            }
            Reply::Empty(200)
        }
        "/api/refresh_sources" => {
            let _g = state_lock();
            tap::refresh_sources();
            Reply::Empty(200)
        }
        "/api/toggle" => {
            let data = loads_obj(body)?;
            let route = data.get("route").cloned().unwrap_or(Value::Null);
            let _g = state_lock();
            tap::toggle_route(&route);
            Reply::Empty(200)
        }
        "/api/heartbeat" => json_reply(&json!({"idle_limit_s": tap::client_timeout_secs()})),
        "/api/toggle_test_tone" | "/api/test_tone" => {
            let data = loads_obj(body)?;
            let freq = data.get("freq").cloned().unwrap_or(json!(1000));
            let restart = data.get("restart");
            {
                let _g = state_lock();
                match restart {
                    Some(Value::Bool(true)) => tap::start_test_tone(&freq)?,
                    Some(Value::Bool(false)) => tap::stop_test_tone(),
                    _ if tap::test_tone_active() => tap::stop_test_tone(),
                    _ => tap::start_test_tone(&freq)?,
                }
            }
            json_reply(&tap::test_tone_json())
        }
        "/api/bitbuddy" => {
            let data = loads_obj(body)?;
            // The page sends the whole setting; anything missing keeps its last value.
            let mut cfg = tap::bitbuddy_config();
            for (k, v) in data.iter() {
                if k != "active" && k != "restart" {
                    cfg.insert(k.clone(), v.clone());
                }
            }
            {
                let _g = state_lock();
                let start = || tap::start_bit_buddy(cfg.clone());
                match data.get("active") {
                    Some(Value::Bool(false)) => tap::stop_bit_buddy(),
                    Some(Value::Bool(true)) => start()?,
                    _ if tap::bitbuddy_active() => {
                        if data.get("restart").map(py::truthy).unwrap_or(false) {
                            start()?
                        } else {
                            tap::stop_bit_buddy()
                        }
                    }
                    _ => start()?,
                }
            }
            json_reply(&tap::bitbuddy_json())
        }
        "/api/bitbuddy/file" => {
            let data = loads_obj(body)?;
            let rep = tap::set_bit_buddy_file(&data)?;
            // A playing file source picks up the new file
            if tap::bitbuddy_active() && tap::bitbuddy_config().get("pattern").and_then(|v| v.as_str()) == Some("file") {
                let _g = state_lock();
                tap::start_bit_buddy(tap::bitbuddy_config())?;
            }
            json_reply(&rep)
        }
        "/api/force_speakers" => {
            {
                let _g = state_lock();
                tap::force_speakers();
            }
            json_reply(&json!({"status": "ok", "speaker_split": true, "physical_sink": tap::physical_sink()}))
        }
        "/api/reset_all" => json_reply(&tap::reset_all()),
        "/api/restart" => {
            tap::full_restart();
            Reply::Empty(200)
        }
        _ => Reply::Empty(404),
    })
}

const POSTS: [&str; 11] = [
    "/api/set_source",
    "/api/refresh_sources",
    "/api/toggle",
    "/api/heartbeat",
    "/api/toggle_test_tone",
    "/api/test_tone",
    "/api/bitbuddy",
    "/api/bitbuddy/file",
    "/api/force_speakers",
    "/api/reset_all",
    "/api/restart",
];

impl Plugin for Patchbay {
    fn name(&self) -> &'static str {
        "patchbay"
    }

    fn start(&self) {
        println!("[WARD BECK TAP] Initializing virtual buses, multi-source meters, and signal tap point...");
        tap::full_restart();
        std::thread::spawn(tap::meter_reader_loop);
        std::thread::spawn(tap::client_watchdog_loop);
    }

    fn get(&self, path: &str, _query: &str) -> Option<RouteResult> {
        Some(Ok(match path {
            "/api/status" => json_reply(&status_payload()),
            "/api/bitbuddy" => {
                let _g = state_lock();
                // The test tone rides along so a page's one generator poll sees both.
                let mut rep = tap::bitbuddy_json();
                if let Some(o) = rep.as_object_mut() {
                    o.insert("test_tone".into(), tap::test_tone_json());
                }
                json_reply(&rep)
            }
            "/api/test_tone" => json_reply(&tap::test_tone_json()),
            _ => return None,
        }))
    }

    fn post(&self, path: &str, body: &str) -> Option<RouteResult> {
        POSTS.contains(&path).then(|| post(path, body))
    }
}

impl services::Patchbay for Patchbay {
    fn refresh_sources(&self) {
        tap::refresh_sources()
    }
    fn apply_routes(&self) {
        tap::apply_routes()
    }
    fn available_sources(&self) -> Vec<Value> {
        tap::available_sources()
    }
    fn is_virtual_sink(&self, name: &str) -> bool {
        tap::is_virtual_sink(name)
    }
    fn physical_sink(&self) -> String {
        tap::physical_sink()
    }
    fn selected_source(&self) -> String {
        tap::selected_source()
    }
    fn set_selected_source(&self, name: &str) {
        tap::set_selected_source(name)
    }
    fn source_meters(&self) -> Value {
        tap::source_meters()
    }
    fn meter_levels(&self) -> Value {
        tap::meter_levels_json()
    }
    fn abandon_ship(&self) {
        let _g = state_lock();
        tap::abandon_ship()
    }
    fn client_checkin(&self) {
        tap::client_checkin()
    }
}

/// Plug the patchbay into the host.
pub fn plug() {
    wbs_plugin::register(&Patchbay);
    services::provide_patchbay(Box::new(Patchbay));
}
