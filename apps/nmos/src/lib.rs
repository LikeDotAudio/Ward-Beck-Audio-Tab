// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! Port of NMOS IS-04 / AoIP discovery in ward-beck-audio-tab.py
//! (NMOS_REGISTRY, DANTE_REGISTRY, RAVENNA_REGISTRY, AOIP_TELEMETRY, nmos_discovery_loop).

// Plugged into the host: routes, services and start-up (plugin.rs).
pub mod plugin;
pub use plugin::plug;

use serde_json::{json, Map, Value};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use wbs_common::{guard, state_lock};
use wbs_plugin::services;
use wbs_pyshim as py;
use wbs_common::{is_shutting_down, now_f64, run_cmd_full};

struct Registries {
    /// {ip: node dict}
    nmos: Map<String, Value>,
    /// {name: device dict}
    dante: Map<String, Value>,
    /// {name: stream dict}
    ravenna: Map<String, Value>,
    /// Complete 4th layer telemetry ({} until the first discovery round)
    aoip: Value,
}

fn reg() -> MutexGuard<'static, Registries> {
    static R: OnceLock<Mutex<Registries>> = OnceLock::new();
    R.get_or_init(|| {
        Mutex::new(Registries {
            nmos: Map::new(),
            dante: Map::new(),
            ravenna: Map::new(),
            aoip: Value::Object(Map::new()),
        })
    })
    .lock()
    .unwrap_or_else(|e| e.into_inner())
}

/// list(NMOS_REGISTRY.values())
pub fn nmos_nodes() -> Vec<Value> {
    reg().nmos.values().cloned().collect()
}

/// len(NMOS_REGISTRY)
pub fn nmos_count() -> usize {
    reg().nmos.len()
}

/// AOIP_TELEMETRY
pub fn aoip_telemetry() -> Value {
    reg().aoip.clone()
}

/// Python `AOIP_TELEMETRY if AOIP_TELEMETRY else inspect_aoip()`.
pub fn aoip_or_inspect() -> Value {
    let t = aoip_telemetry();
    if py::truthy(&t) {
        t
    } else {
        services::pipeline().inspect_aoip(true)
    }
}

/// Dante device IPs from the telemetry (aoip_listener's Dante IP provider).
pub fn dante_ips() -> Vec<String> {
    let t = aoip_telemetry();
    t.get("dante")
        .and_then(|d| d.get("devices"))
        .and_then(|d| d.as_array())
        .map(|a| a.iter().filter_map(|d| d.get("ip").and_then(|i| i.as_str()).map(String::from)).collect())
        .unwrap_or_default()
}

/// Discover AoIP Network Announcements: NMOS IS-04, Dante, Ravenna, AES67 SAP, SIP.
pub fn discover_aoip_network() {
    discover_nmos_nodes();
    let Some(aoip_data) = guard(|| services::pipeline().inspect_aoip(true)) else { return };
    let _g = state_lock();
    let mut r = reg();
    r.aoip = aoip_data.clone();
    let list = |k: &str, k2: &str| -> Vec<Value> {
        aoip_data.get(k).and_then(|d| d.get(k2)).and_then(|d| d.as_array()).cloned().unwrap_or_default()
    };
    for dev in list("dante", "devices") {
        let Some(name) = dev.get("name") else { return }; // Python: KeyError ends the round
        r.dante.insert(py::str_of(name), dev.clone());
    }
    for rst in list("ravenna", "streams") {
        let Some(name) = rst.get("name") else { return };
        r.ravenna.insert(py::str_of(name), rst.clone());
    }
}

/// `avahi-browse -r -t -p _nmos-node._tcp` resolved lines -> [{name, host, ip, port}].
pub fn parse_avahi(out: &str) -> Vec<Value> {
    let mut nodes = Vec::new();
    for line in out.lines() {
        let parts: Vec<&str> = line.split(';').collect();
        if parts.len() >= 9 && parts[0] == "=" {
            let Ok(port) = py::parse_int(parts[8]) else { break }; // Python: ValueError ends the scan
            nodes.push(json!({"name": parts[3], "host": parts[6], "ip": parts[7], "port": port}));
        }
    }
    nodes
}

fn fetch_json(url: &str) -> Option<Value> {
    let body = services::cbc().get(
        url,
        &[("Accept", "application/json"), ("User-Agent", "Ward-Beck-NMOS-Browser")],
        1.2,
    )
    .ok()?;
    let s = String::from_utf8(body).ok()?;
    serde_json::from_str(&s).ok()
}

/// Discover NMOS IS-04 nodes announcing via mDNS or known subnets.
pub fn discover_nmos_nodes() {
    let mut discovered = Map::new();

    // 1. Probe mDNS for _nmos-node._tcp
    let mut nodes_found = match run_cmd_full(&["avahi-browse", "-r", "-t", "-p", "_nmos-node._tcp"], Duration::from_secs(2)) {
        Some((out, 0)) => parse_avahi(&out),
        _ => Vec::new(),
    };

    // 2. Known/fallback targets on broadcast subnet
    let known_targets = [("WBS-32me", "WBS-32me.local", "44.44.44.158", 80)];
    for (name, host, ip, port) in known_targets {
        if !nodes_found.iter().any(|n| n["ip"] == ip) {
            nodes_found.push(json!({"name": name, "host": host, "ip": ip, "port": port}));
        }
    }

    // 3. Query IS-04 Node API for each node
    for node in &nodes_found {
        let ip = node["ip"].as_str().unwrap_or("").to_string();
        let port = node["port"].clone();
        let base_urls = [
            format!("http://{ip}:{port}/x-nmos/v1.0/node"),
            format!("http://{ip}:{port}/x-nmos/node/v1.3"),
            format!("http://{ip}:{port}/x-nmos/node/v1.2"),
        ];
        let mut node_self = None;
        let mut used_base = None;
        for b in &base_urls {
            if let Some(v) = fetch_json(&format!("{b}/self")) {
                node_self = Some(v);
                used_base = Some(b.clone());
                break;
            }
        }
        let (Some(node_self), Some(used_base)) = (node_self, used_base) else { continue };
        if !py::truthy(&node_self) {
            continue;
        }
        let Some(ns) = node_self.as_object() else { continue };

        let mut lists: Vec<Vec<Value>> = Vec::new();
        for ep in ["sources", "flows", "senders"] {
            let mut trg = Vec::new();
            match fetch_json(&format!("{used_base}/{ep}")) {
                Some(Value::Array(a)) => trg.extend(a),
                Some(Value::Object(o)) => trg.extend(o.keys().map(|k| Value::String(k.clone()))),
                _ => {}
            }
            lists.push(trg);
        }
        let mut m = Map::new();
        m.insert("id".into(), ns.get("id").cloned().unwrap_or(Value::Null));
        m.insert("label".into(), ns.get("label").cloned().unwrap_or_else(|| node["name"].clone()));
        m.insert("hostname".into(), ns.get("hostname").cloned().unwrap_or_else(|| node["host"].clone()));
        m.insert("ip".into(), json!(ip));
        m.insert("port".into(), port);
        m.insert("api_base".into(), json!(used_base));
        m.insert("node".into(), node_self.clone());
        m.insert("sources".into(), Value::Array(std::mem::take(&mut lists[0])));
        m.insert("flows".into(), Value::Array(std::mem::take(&mut lists[1])));
        m.insert("senders".into(), Value::Array(std::mem::take(&mut lists[2])));
        m.insert("last_seen".into(), json!(now_f64()));
        discovered.insert(ip, Value::Object(m));
    }

    let _g = state_lock();
    reg().nmos = discovered;
}

/// Background loop refreshing NMOS, Dante, Ravenna announcements and CBC Test Generator every 5 seconds.
pub fn nmos_discovery_loop() {
    while !is_shutting_down() {
        let _ = guard(|| {
            discover_aoip_network();
            services::cbc().poll_generator();
            services::patchbay().refresh_sources();
        });
        for _ in 0..50 {
            if is_shutting_down() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avahi_lines() {
        let out = "+;eth0;IPv4;WBS-32me;_nmos-node._tcp;local\n\
                   =;eth0;IPv4;WBS-32me;_nmos-node._tcp;local;WBS-32me.local;44.44.44.158;80;\"api_ver=v1.3\"\n";
        let n = parse_avahi(out);
        assert_eq!(py::dumps(&Value::Array(n)), r#"[{"name": "WBS-32me", "host": "WBS-32me.local", "ip": "44.44.44.158", "port": 80}]"#);
    }
}
