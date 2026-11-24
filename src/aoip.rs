// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! Port of aoip_listener.py: SAP/SDP, PTP (v1/v2), SIP multicast listeners and PTP over MQTT.
//! CONTRACT: keep these pub signatures. Parsers may be pub for tests.
//!
//! Joins the real multicast groups used by Audio-over-IP networks and decodes
//! what arrives on the wire:
//!
//!   - SAP / SDP (RFC 2974 / RFC 4566): 239.255.255.255:9875, 224.2.127.254:9875
//!     (AES67, RAVENNA, Dante in AES67 mode)
//!   - PTP: 224.0.1.129 UDP 319 (event) and 320 (general)
//!     PTPv2 / IEEE 1588-2008 (AES67, RAVENNA, Dante AES67 mode)
//!     PTPv1 / IEEE 1588-2002 (Dante native clocking)
//!   - SIP: 224.0.1.75:5060 (sip.mcast.net)
//!
//! Nothing here is simulated: if a socket cannot bind or join, the listener
//! reports listening=false with the error text and an empty list.
//!
//! PTP comes from the APK PTP sniffer built in (crate apkaudio-ptp, submodule PTP/):
//! PTPv1 + PTPv2 on UDP 319/320 and gPTP on raw Ethernet, listen-only. It needs
//! cap_net_bind_service + cap_net_raw on this binary; without them the snapshot
//! carries a `fix` prompt. AOIP_PTP_MODE=mqtt reads the apk-ptp container's topic
//! instead, AOIP_PTP_MODE=sockets the old built-in 319/320 listener.
//!
//! Environment:
//!   AOIP_IFACE      optional IPv4 address of the interface to join on
//!                   (default: join on every interface, reported as 0.0.0.0).
//!   AOIP_PTP_MODE   "apk" (default, built-in sniffer), "mqtt" or "sockets".
//!   MQTT_HOST/PORT  broker for the passive PTP source (127.0.0.1:1883).
//!   AOIP_PTP_TOPIC  topic to read (APK.audio/System/Gui/Discovered/ptp).

pub(crate) mod inflate;
pub(crate) mod py;

use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use crate::util::now_f64;

pub const SAP_GROUPS: [(&str, u16); 2] = [("239.255.255.255", 9875), ("224.2.127.254", 9875)];
pub const PTP_GROUP: &str = "224.0.1.129";
pub const PTP_PORTS: [u16; 2] = [319, 320];
pub const SIP_GROUP: (&str, u16) = ("224.0.1.75", 5060);

pub const SAP_EXPIRY_S: f64 = 300.0;
pub const PTP_EXPIRY_S: f64 = 30.0;
pub const SIP_MAX_ENTRIES: usize = 50;

type Provider = Box<dyn Fn() -> Vec<String> + Send + Sync>;

// ---------------------------------------------------------------------------
// Configuration (read once, like the Python module globals)
// ---------------------------------------------------------------------------

struct Config {
    ptp_mode: String,
    mqtt_host: String,
    mqtt_port: u16,
    ptp_topic: String,
}

fn config() -> &'static Config {
    static C: OnceLock<Config> = OnceLock::new();
    C.get_or_init(|| Config {
        ptp_mode: std::env::var("AOIP_PTP_MODE").unwrap_or_else(|_| "apk".into()).trim().to_lowercase(),
        mqtt_host: std::env::var("MQTT_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        mqtt_port: std::env::var("MQTT_PORT").ok().and_then(|v| py::parse_int(&v)).map(|v| v as u16).unwrap_or(1883),
        ptp_topic: std::env::var("AOIP_PTP_TOPIC").unwrap_or_else(|_| "APK.audio/System/Gui/Discovered/ptp".into()),
    })
}

fn sockets_mode() -> bool {
    config().ptp_mode == "sockets"
}

fn apk_mode() -> bool {
    !matches!(config().ptp_mode.as_str(), "sockets" | "mqtt")
}

fn iface() -> String {
    let v = std::env::var("AOIP_IFACE").unwrap_or_default();
    let v = v.trim();
    if v.is_empty() { "0.0.0.0".into() } else { v.to_string() }
}

fn fmt_id(raw: &[u8]) -> String {
    raw.iter().map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join("-")
}

fn be16(d: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([d[i], d[i + 1]])
}

fn nul_str(d: &[u8]) -> String {
    let end = d.iter().position(|&b| b == 0).unwrap_or(d.len());
    py::decode_ascii(&d[..end])
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ListenerState {
    listening: bool,
    error: Option<String>,
    groups: Vec<String>,
    packets: u64,
    joined_interfaces: Vec<String>,
    join_errors: Vec<String>,
}

impl ListenerState {
    fn new(groups: Vec<String>) -> Self {
        ListenerState { listening: false, error: None, groups, packets: 0, joined_interfaces: vec![], join_errors: vec![] }
    }
    fn public(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("listening".into(), json!(self.listening));
        m.insert("error".into(), json!(self.error));
        m.insert("groups".into(), json!(self.groups));
        m.insert("packets".into(), json!(self.packets));
        m.insert("joined_interfaces".into(), json!(self.joined_interfaces));
        m.insert("join_errors".into(), json!(self.join_errors));
        m
    }
}

/// Insertion-ordered map with Python dict semantics (update keeps position, pop + insert appends).
#[derive(Debug, Clone, Default)]
struct OrderedMap<K: PartialEq, V> {
    items: Vec<(K, V)>,
}

impl<K: PartialEq, V> OrderedMap<K, V> {
    fn get_mut(&mut self, k: &K) -> Option<&mut V> {
        self.items.iter_mut().find(|(kk, _)| kk == k).map(|(_, v)| v)
    }
    fn get(&self, k: &K) -> Option<&V> {
        self.items.iter().find(|(kk, _)| kk == k).map(|(_, v)| v)
    }
    fn insert(&mut self, k: K, v: V) {
        match self.get_mut(&k) {
            Some(slot) => *slot = v,
            None => self.items.push((k, v)),
        }
    }
    fn remove(&mut self, k: &K) {
        self.items.retain(|(kk, _)| kk != k);
    }
    fn len(&self) -> usize {
        self.items.len()
    }
    fn values(&self) -> impl Iterator<Item = &V> {
        self.items.iter().map(|(_, v)| v)
    }
    fn clear(&mut self) {
        self.items.clear();
    }
}

#[derive(Debug, Clone, Default)]
struct MqttState {
    connected: bool,
    error: Option<String>,
    /// Built-in sniffer only: the runtime prompt when it can't hear the wire.
    fix: Option<Value>,
    /// Built-in sniffer only: what changed, newest last (ptp_events).
    events: std::collections::VecDeque<Value>,
    messages: u64,
    updated: Option<f64>,
}

/// Everything heard on the multicast groups (Python: the module-level dicts).
pub struct Registry {
    sap: ListenerState,
    sap_sessions: OrderedMap<(String, u16), Map<String, Value>>,
    ptp: ListenerState,
    ptp_errors: BTreeMap<u16, String>,
    ptp_bound: BTreeSet<u16>,
    ptp_clocks: OrderedMap<(i64, Value, String), Map<String, Value>>,
    sip: ListenerState,
    sip_messages: OrderedMap<(String, Value), Map<String, Value>>,
    mqtt: MqttState,
    mqtt_rows: Vec<Value>,
    /// None = follow AOIP_PTP_MODE; Some(true) = sockets; Some(false) = mqtt (tests).
    ptp_sockets_override: Option<bool>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        Registry {
            sap: ListenerState::new(SAP_GROUPS.iter().map(|(g, p)| format!("{g}:{p}")).collect()),
            sap_sessions: OrderedMap::default(),
            ptp: ListenerState::new(PTP_PORTS.iter().map(|p| format!("{PTP_GROUP}:{p}")).collect()),
            ptp_errors: BTreeMap::new(),
            ptp_bound: BTreeSet::new(),
            ptp_clocks: OrderedMap::default(),
            sip: ListenerState::new(vec![format!("{}:{}", SIP_GROUP.0, SIP_GROUP.1)]),
            sip_messages: OrderedMap::default(),
            mqtt: MqttState::default(),
            mqtt_rows: vec![],
            ptp_sockets_override: None,
        }
    }

    /// Pin the PTP source (tests: Python sets al.PTP_MODE = "sockets").
    pub fn set_ptp_sockets_mode(&mut self, sockets: bool) {
        self.ptp_sockets_override = Some(sockets);
    }

    pub fn clear(&mut self) {
        self.sap_sessions.clear();
        self.ptp_clocks.clear();
        self.sip_messages.clear();
    }

    fn ptp_sockets(&self) -> bool {
        self.ptp_sockets_override.unwrap_or_else(sockets_mode)
    }

    // -- SAP --
    pub fn handle_sap(&mut self, data: &[u8], src_ip: &str, now: f64) {
        let pkt = match parse_sap(data) {
            Ok(p) => p,
            Err(_) => return,
        };
        let origin_ip = pkt["origin_ip"].as_str().unwrap_or("").to_string();
        let hash = pkt["msg_id_hash"].as_u64().unwrap_or(0) as u16;
        let key = (origin_ip.clone(), hash);
        if pkt["deletion"].as_bool().unwrap_or(false) {
            self.sap_sessions.remove(&key);
            // A deletion may carry a different hash; also match the o= line.
            let origin = pkt["sdp"].get("origin").cloned().unwrap_or(Value::Null);
            if py::truthy(&origin) {
                self.sap_sessions.items.retain(|(_, s)| s.get("origin") != Some(&origin));
            }
            return;
        }
        let sdp = match pkt["sdp"].as_object() {
            Some(s) => s,
            None => return,
        };
        let mut s = sdp.clone();
        s.insert("id".into(), json!(format!("{}#{:04x}", origin_ip, hash)));
        s.insert("origin_ip".into(), json!(origin_ip));
        s.insert("source_ip".into(), json!(src_ip));
        s.insert("last_seen".into(), json!(now));
        let count = self
            .sap_sessions
            .get(&key)
            .and_then(|p| p.get("announce_count"))
            .and_then(|v| v.as_i64())
            .map(|c| c + 1)
            .unwrap_or(1);
        s.insert("announce_count".into(), json!(count));
        self.sap_sessions.insert(key, s);
    }

    // -- PTP (sockets) --
    pub fn handle_ptp(&mut self, data: &[u8], src_ip: &str, now: f64) {
        let m = match parse_ptp(data) {
            Ok(m) => m,
            Err(_) => return,
        };
        let version = m["version"].as_i64().unwrap_or(0);
        let domain = if version == 2 { m["domain"].clone() } else { m["subdomain"].clone() };
        let cid = m["clock_identity"].as_str().unwrap_or("").to_string();
        let key = (version, domain.clone(), cid.clone());
        if self.ptp_clocks.get(&key).is_none() {
            let mut c = Map::new();
            c.insert("clock_identity".into(), json!(cid));
            c.insert("version".into(), json!(version));
            c.insert("domain".into(), domain);
            c.insert("source_ip".into(), json!(src_ip));
            c.insert("network".into(), json!(if version == 1 { "dante" } else { "ravenna/aes67" }));
            c.insert("grandmaster_identity".into(), Value::Null);
            c.insert("is_grandmaster".into(), json!(false));
            for k in ["priority1", "priority2", "clock_class", "clock_accuracy", "offset_scaled_log_variance", "steps_removed", "time_source"] {
                c.insert(k.into(), Value::Null);
            }
            for k in ["sync_count", "follow_up_count", "delay_req_count", "announce_count", "other_count"] {
                c.insert(k.into(), json!(0));
            }
            c.insert("last_message".into(), Value::Null);
            c.insert("last_seen".into(), json!(now));
            self.ptp_clocks.insert(key.clone(), c);
        }
        let c = self.ptp_clocks.get_mut(&key).expect("clock just inserted");
        c.insert("source_ip".into(), json!(src_ip));
        c.insert("last_seen".into(), json!(now));
        c.insert("last_message".into(), m["message_name"].clone());
        let name = m["message_name"].as_str().unwrap_or("");
        let counter = match name {
            "Sync" => "sync_count",
            "Follow_Up" => "follow_up_count",
            "Delay_Req" => "delay_req_count",
            "Announce" => "announce_count",
            _ => "other_count",
        };
        let n = c.get(counter).and_then(|v| v.as_i64()).unwrap_or(0);
        c.insert(counter.into(), json!(n + 1));
        if let Some(a) = m.get("announce").and_then(|a| a.as_object()) {
            for f in ["priority1", "priority2", "clock_class", "clock_accuracy", "offset_scaled_log_variance", "steps_removed", "time_source", "grandmaster_identity"] {
                c.insert(f.into(), a[f].clone());
            }
            c.insert("is_grandmaster".into(), json!(a["grandmaster_identity"] == m["clock_identity"]));
        }
        if let Some(s) = m.get("sync").and_then(|s| s.as_object()) {
            if name == "Sync" {
                c.insert("grandmaster_identity".into(), s["grandmaster_identity"].clone());
                c.insert("steps_removed".into(), s["steps_removed"].clone());
                c.insert("clock_class".into(), s["grandmaster_stratum"].clone());
                c.insert("clock_identifier".into(), s["grandmaster_identifier"].clone());
                c.insert("grandmaster_preferred".into(), s["grandmaster_preferred"].clone());
                c.insert("is_grandmaster".into(), json!(s["grandmaster_identity"] == m["clock_identity"]));
            }
        }
    }

    // -- SIP --
    pub fn handle_sip(&mut self, data: &[u8], src_ip: &str, now: f64) {
        let m = match parse_sip(data) {
            Ok(m) => m,
            Err(_) => return,
        };
        let method = m["method"].as_str().unwrap_or("").to_string();
        let key = (method.clone(), m["from"].clone());
        if self.sip_messages.get(&key).is_none() {
            let mut e = Map::new();
            e.insert("method".into(), json!(method));
            e.insert("from".into(), m["from"].clone());
            e.insert("count".into(), json!(0));
            self.sip_messages.insert(key.clone(), e);
        }
        let e = self.sip_messages.get_mut(&key).expect("entry just inserted");
        for (k, src) in [("to", "to"), ("call_id", "call_id"), ("contact", "contact"), ("user_agent", "user_agent"), ("start_line", "start_line")] {
            e.insert(k.into(), m[src].clone());
        }
        e.insert("source_ip".into(), json!(src_ip));
        e.insert("last_seen".into(), json!(now));
        let n = e.get("count").and_then(|v| v.as_i64()).unwrap_or(0);
        e.insert("count".into(), json!(n + 1));
        if self.sip_messages.len() > SIP_MAX_ENTRIES {
            let excess = self.sip_messages.len() - SIP_MAX_ENTRIES;
            let mut order: Vec<(f64, usize)> = self
                .sip_messages
                .items
                .iter()
                .enumerate()
                .map(|(i, (_, e))| (e["last_seen"].as_f64().unwrap_or(0.0), i))
                .collect();
            order.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            let drop: HashSet<usize> = order.iter().take(excess).map(|(_, i)| *i).collect();
            let mut i = 0;
            self.sip_messages.items.retain(|_| {
                let keep = !drop.contains(&i);
                i += 1;
                keep
            });
        }
    }

    fn prune(&mut self, now: f64) {
        let age = |m: &Map<String, Value>| now - m.get("last_seen").and_then(|v| v.as_f64()).unwrap_or(0.0);
        // `!(a > b)` keeps NaN ages, like the Python comparison
        #[allow(clippy::neg_cmp_op_on_partial_ord)]
        self.sap_sessions.items.retain(|(_, s)| !(age(s) > SAP_EXPIRY_S));
        #[allow(clippy::neg_cmp_op_on_partial_ord)]
        self.ptp_clocks.items.retain(|(_, c)| !(age(c) > PTP_EXPIRY_S));
    }

    fn ptp_mqtt_snapshot(&self, now: f64) -> Value {
        let cfg = config();
        let online: Vec<&Value> = self.mqtt_rows.iter().filter(|c| c["online"].as_bool().unwrap_or(false)).collect();
        let mut gms: Vec<Value> = online
            .iter()
            .filter(|c| {
                // PTPv1 (Dante) masters don't announce a GM role; the clock sending Sync is the master.
                c["is_grandmaster"].as_bool().unwrap_or(false)
                    || (c["version"].as_i64() == Some(1) && c["sync_count"].as_i64().unwrap_or(0) > 0)
            })
            .map(|c| {
                let mut g = Map::new();
                g.insert("version".into(), c["version"].clone());
                g.insert("domain".into(), c["domain"].clone());
                g.insert("network".into(), c["network"].clone());
                g.insert("identity".into(), c["clock_identity"].clone());
                g.insert("source_ip".into(), c["source_ip"].clone());
                g.insert("priority1".into(), c["priority1"].clone());
                g.insert("sync_health".into(), c["sync_health"].clone());
                g.insert("interface".into(), c["interface"].clone());
                g.insert("ptp_offset".into(), c["ptp_offset"].clone());
                Value::Object(g)
            })
            .collect();
        gms.sort_by(|a, b| {
            let ka = (a["version"].as_i64().unwrap_or(0), py::str_of(&a["domain"]), a["priority1"].as_i64().unwrap_or(999));
            let kb = (b["version"].as_i64().unwrap_or(0), py::str_of(&b["domain"]), b["priority1"].as_i64().unwrap_or(999));
            ka.cmp(&kb)
        });
        let upd = self.mqtt.updated;
        let upd_v = upd.map(|u| json!(u)).unwrap_or(Value::Null);
        let clocks: Vec<Value> = self
            .mqtt_rows
            .iter()
            .map(|c| {
                let mut d = c.as_object().cloned().unwrap_or_default();
                d.insert("age_s".into(), Value::Null);
                d.insert("last_seen".into(), upd_v.clone());
                Value::Object(d)
            })
            .collect();
        let mut m = Map::new();
        m.insert("listening".into(), json!(self.mqtt.connected));
        m.insert("error".into(), json!(self.mqtt.error));
        // A pinned mode (tests) means the MQTT source; otherwise follow AOIP_PTP_MODE.
        if self.ptp_sockets_override.is_none() && apk_mode() {
            m.insert("mode".into(), json!("apk-ptp"));
            m.insert("source".into(), json!("apk-ptp"));
            m.insert("groups".into(), json!([format!("{PTP_GROUP}:319"), format!("{PTP_GROUP}:320"), "gPTP 0x88F7"]));
            m.insert("fix".into(), self.mqtt.fix.clone().unwrap_or(Value::Null));
            m.insert("events".into(), Value::Array(self.mqtt.events.iter().cloned().collect()));
        } else {
            m.insert("mode".into(), json!("passive"));
            m.insert("source".into(), json!(format!("mqtt://{}:{}", cfg.mqtt_host, cfg.mqtt_port)));
            m.insert("groups".into(), json!([cfg.ptp_topic]));
            m.insert("fix".into(), Value::Null);
            m.insert("events".into(), json!([]));
        }
        m.insert("packets".into(), json!(self.mqtt.messages));
        m.insert("joined_interfaces".into(), json!([]));
        m.insert("join_errors".into(), json!([]));
        m.insert("bound_ports".into(), json!([]));
        m.insert(
            "updated_age_s".into(),
            match upd {
                Some(u) if u != 0.0 => json!(py::round(now - u, 1)),
                _ => Value::Null,
            },
        );
        m.insert("clocks".into(), Value::Array(clocks));
        m.insert("grandmasters".into(), Value::Array(gms));
        Value::Object(m)
    }

    /// JSON-ready view of everything heard (Python: snapshot()).
    pub fn snapshot_at(&mut self, now: f64, dante: &HashSet<String>) -> Value {
        self.prune(now);
        let age = |m: &Map<String, Value>| py::round(now - m.get("last_seen").and_then(|v| v.as_f64()).unwrap_or(0.0), 1);
        let by_age = |v: &mut Vec<Value>| {
            v.sort_by(|a, b| a["age_s"].as_f64().unwrap_or(0.0).partial_cmp(&b["age_s"].as_f64().unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal))
        };

        let mut sap = self.sap.public();
        let mut sessions: Vec<Value> = self
            .sap_sessions
            .values()
            .map(|s| {
                let mut d = s.clone();
                d.insert("age_s".into(), json!(age(s)));
                Value::Object(d)
            })
            .collect();
        by_age(&mut sessions);
        sap.insert("sessions".into(), Value::Array(sessions));

        let mut ptp = self.ptp.public();
        ptp.insert("listening".into(), json!(!self.ptp_bound.is_empty()));
        ptp.insert("bound_ports".into(), json!(self.ptp_bound.iter().collect::<Vec<_>>()));
        let mut clocks: Vec<Map<String, Value>> = self
            .ptp_clocks
            .values()
            .map(|c| {
                let mut d = c.clone();
                if d["version"].as_i64() == Some(2) && d["source_ip"].as_str().map(|ip| dante.contains(ip)).unwrap_or(false) {
                    d.insert("network".into(), json!("dante (aes67 mode)"));
                }
                d.insert("age_s".into(), json!(age(c)));
                d
            })
            .collect();
        let ckey = |c: &Map<String, Value>| {
            (c["version"].as_i64().unwrap_or(0), py::str_of(&c["domain"]), c["clock_identity"].as_str().unwrap_or("").to_string())
        };
        clocks.sort_by_key(|a| ckey(a));

        // grandmasters, keyed by (version, domain), built in last_seen order
        let mut by_last: Vec<&Map<String, Value>> = clocks.iter().collect();
        by_last.sort_by(|a, b| {
            a["last_seen"].as_f64().unwrap_or(0.0).partial_cmp(&b["last_seen"].as_f64().unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut gms: OrderedMap<(i64, Value), Value> = OrderedMap::default();
        for c in by_last {
            let gid = &c["grandmaster_identity"];
            if !py::truthy(gid) {
                continue;
            }
            let ver = c["version"].as_i64().unwrap_or(0);
            let gm_clock = clocks.iter().rev().find(|o| {
                o["version"].as_i64() == Some(ver) && o["domain"] == c["domain"] && &o["clock_identity"] == gid
            });
            let src = gm_clock.unwrap_or(c);
            let mut g = Map::new();
            g.insert("version".into(), json!(ver));
            g.insert("domain".into(), c["domain"].clone());
            g.insert("network".into(), src["network"].clone());
            g.insert("identity".into(), gid.clone());
            g.insert(
                "source_ip".into(),
                if gm_clock.is_some() || c["is_grandmaster"].as_bool().unwrap_or(false) { src["source_ip"].clone() } else { Value::Null },
            );
            g.insert("announced_by".into(), c["source_ip"].clone());
            gms.insert((ver, c["domain"].clone()), Value::Object(g));
        }
        let mut gm_list: Vec<(i64, String, Value)> =
            gms.items.into_iter().map(|((v, d), g)| (v, py::str_of(&d), g)).collect();
        gm_list.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        ptp.insert("clocks".into(), Value::Array(clocks.into_iter().map(Value::Object).collect()));
        ptp.insert("grandmasters".into(), Value::Array(gm_list.into_iter().map(|(_, _, g)| g).collect()));
        ptp.insert("mode".into(), json!("sockets"));
        let ptp = if self.ptp_sockets() { Value::Object(ptp) } else { self.ptp_mqtt_snapshot(now) };

        let mut sip = self.sip.public();
        let mut msgs: Vec<Value> = self
            .sip_messages
            .values()
            .map(|e| {
                let mut d = e.clone();
                d.insert("age_s".into(), json!(age(e)));
                Value::Object(d)
            })
            .collect();
        by_age(&mut msgs);
        sip.insert("messages".into(), Value::Array(msgs));

        let mut out = Map::new();
        out.insert("iface".into(), json!(iface()));
        out.insert("sap".into(), Value::Object(sap));
        out.insert("ptp".into(), ptp);
        out.insert("sip".into(), Value::Object(sip));
        Value::Object(out)
    }
}

fn registry() -> MutexGuard<'static, Registry> {
    static R: OnceLock<Mutex<Registry>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Registry::new())).lock().unwrap_or_else(|e| e.into_inner())
}

fn provider() -> &'static Mutex<Option<Arc<Provider>>> {
    static P: OnceLock<Mutex<Option<Arc<Provider>>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(None))
}

/// fn() -> Dante device IPs (used to tag PTPv2 clocks as Dante AES67 mode).
pub fn set_dante_ip_provider(f: Box<dyn Fn() -> Vec<String> + Send + Sync>) {
    *provider().lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(f));
}

/// Remove the provider (Python: set_dante_ip_provider(None)).
pub fn clear_dante_ip_provider() {
    *provider().lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn dante_ips() -> HashSet<String> {
    let p = provider().lock().unwrap_or_else(|e| e.into_inner()).clone();
    match p {
        None => HashSet::new(),
        Some(f) => std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f()))
            .map(|v| v.into_iter().filter(|ip| !ip.is_empty()).collect())
            .unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// SAP / SDP
// ---------------------------------------------------------------------------

pub fn parse_sdp(text: &str) -> Value {
    let mut out = Map::new();
    for k in [
        "session_name", "origin", "origin_address", "multicast_ip", "ttl", "port", "encoding", "sample_rate",
        "channels", "ptime", "ptp_version", "ptp_grandmaster", "ptp_domain", "mediaclk",
    ] {
        out.insert(k.into(), Value::Null);
    }
    let text = text.replace("\r\n", "\n");
    for raw in text.split('\n') {
        let line = py::strip(raw);
        let mut chars = line.chars();
        let (k, eq) = (chars.next(), chars.next());
        if line.chars().count() < 2 || eq != Some('=') {
            continue;
        }
        let k = k.unwrap_or(' ');
        let v = py::strip(&line[k.len_utf8() + 1..]);
        match k {
            's' => {
                out.insert("session_name".into(), json!(v));
            }
            'o' => {
                out.insert("origin".into(), json!(v));
                let parts = py::split_ws(v);
                if parts.len() >= 6 {
                    out.insert("origin_address".into(), json!(parts[5]));
                }
            }
            'c' => {
                let parts = py::split_ws(v);
                if parts.len() >= 3 && out["multicast_ip"].is_null() {
                    let addr: Vec<&str> = parts[2].split('/').collect();
                    out.insert("multicast_ip".into(), json!(addr[0]));
                    if addr.len() > 1 && py::is_digits(addr[1]) {
                        if let Ok(t) = addr[1].parse::<i64>() {
                            out.insert("ttl".into(), json!(t));
                        }
                    }
                }
            }
            'm' => {
                let parts = py::split_ws(v);
                if parts.len() >= 2 && parts[0] == "audio" && out["port"].is_null() {
                    if let Some(p) = py::parse_int(parts[1].split('/').next().unwrap_or("")) {
                        out.insert("port".into(), json!(p));
                    }
                }
            }
            'a' => {
                if let Some(rest) = v.strip_prefix("rtpmap:") {
                    if out["encoding"].is_null() {
                        let spec = py::split_ws_once(rest);
                        if spec.len() == 2 {
                            let enc: Vec<&str> = spec[1].split('/').collect();
                            out.insert("encoding".into(), json!(enc[0]));
                            if enc.len() > 1 && py::is_digits(enc[1]) {
                                if let Ok(r) = enc[1].parse::<i64>() {
                                    out.insert("sample_rate".into(), json!(r));
                                }
                            }
                            let ch = if enc.len() > 2 && py::is_digits(enc[2]) { enc[2].parse::<i64>().unwrap_or(1) } else { 1 };
                            out.insert("channels".into(), json!(ch));
                        }
                    }
                } else if let Some(rest) = v.strip_prefix("ptime:") {
                    if let Some(f) = py::parse_float(rest) {
                        out.insert("ptime".into(), json!(f));
                    }
                } else if let Some(rest) = v.strip_prefix("mediaclk:") {
                    out.insert("mediaclk".into(), json!(rest));
                } else if let Some(rest) = v.strip_prefix("ts-refclk:ptp=") {
                    // ptp=IEEE1588-2008:00-1D-C1-FF-FE-12-34-56:0  (or ...:traceable)
                    let fields: Vec<&str> = rest.split(':').collect();
                    out.insert("ptp_version".into(), json!(fields[0]));
                    if fields.len() > 1 && fields[1] != "traceable" {
                        out.insert("ptp_grandmaster".into(), json!(fields[1].to_uppercase()));
                    } else if fields.len() > 1 {
                        out.insert("ptp_grandmaster".into(), json!("traceable"));
                    }
                    if fields.len() > 2 {
                        if let Some(d) = py::parse_int(fields[2]) {
                            out.insert("ptp_domain".into(), json!(d));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Value::Object(out)
}

/// Decode a SAP packet (RFC 2974). Err on malformed input.
pub fn parse_sap(data: &[u8]) -> Result<Value, String> {
    if data.len() < 8 {
        return Err("SAP packet too short".into());
    }
    let b0 = data[0];
    let version = b0 >> 5;
    let ipv6 = b0 & 0x10 != 0;
    let deletion = b0 & 0x04 != 0;
    let encrypted = b0 & 0x02 != 0;
    let compressed = b0 & 0x01 != 0;
    let auth_len = data[1] as usize * 4;
    let msg_id_hash = be16(data, 2);
    let alen = if ipv6 { 16 } else { 4 };
    let mut pos = 4;
    if data.len() < pos + alen {
        return Err("SAP origin truncated".into());
    }
    let origin_ip = py::ntop(&data[pos..pos + alen]);
    pos += alen + auth_len;
    let mut payload: Vec<u8> = data.get(pos..).map(|s| s.to_vec()).unwrap_or_default();
    if compressed {
        payload = inflate::zlib_decompress(&payload)?;
    }
    let mut payload_type = "application/sdp".to_string();
    if !payload.starts_with(b"v=0") {
        if let Some(nul) = payload.iter().position(|&b| b == 0) {
            payload_type = py::decode_ascii(&payload[..nul]);
            payload = payload[nul + 1..].to_vec();
        }
    }
    let mut res = Map::new();
    res.insert("version".into(), json!(version));
    res.insert("deletion".into(), json!(deletion));
    res.insert("encrypted".into(), json!(encrypted));
    res.insert("compressed".into(), json!(compressed));
    res.insert("msg_id_hash".into(), json!(msg_id_hash));
    res.insert("origin_ip".into(), json!(origin_ip));
    res.insert("payload_type".into(), json!(payload_type));
    res.insert("sdp".into(), Value::Null);
    if !encrypted && payload_type == "application/sdp" {
        res.insert("sdp".into(), parse_sdp(&String::from_utf8_lossy(&payload)));
    }
    Ok(Value::Object(res))
}

// ---------------------------------------------------------------------------
// PTP
// ---------------------------------------------------------------------------

fn ptp2_type(t: u8) -> String {
    match t {
        0x0 => "Sync".into(),
        0x1 => "Delay_Req".into(),
        0x2 => "Pdelay_Req".into(),
        0x3 => "Pdelay_Resp".into(),
        0x8 => "Follow_Up".into(),
        0x9 => "Delay_Resp".into(),
        0xA => "Pdelay_Resp_Follow_Up".into(),
        0xB => "Announce".into(),
        0xC => "Signaling".into(),
        0xD => "Management".into(),
        _ => format!("0x{:X}", t),
    }
}

fn ptp1_control(c: u8) -> String {
    match c {
        0 => "Sync".into(),
        1 => "Delay_Req".into(),
        2 => "Follow_Up".into(),
        3 => "Delay_Resp".into(),
        4 => "Management".into(),
        _ => format!("0x{:X}", c),
    }
}

fn time_source(t: u8) -> String {
    match t {
        0x10 => "ATOMIC_CLOCK".into(),
        0x20 => "GPS".into(),
        0x30 => "TERRESTRIAL_RADIO".into(),
        0x40 => "PTP".into(),
        0x50 => "NTP".into(),
        0x60 => "HAND_SET".into(),
        0x90 => "OTHER".into(),
        0xA0 => "INTERNAL_OSCILLATOR".into(),
        _ => format!("0x{:02X}", t),
    }
}

/// Decode a PTPv1 or PTPv2 message. Err if neither.
pub fn parse_ptp(data: &[u8]) -> Result<Value, String> {
    if data.len() >= 40 && be16(data, 0) == 1 {
        return Ok(parse_ptp1(data));
    }
    if data.len() >= 34 && (data[1] & 0x0F) == 2 {
        return Ok(parse_ptp2(data));
    }
    Err("not a PTPv1/v2 message".into())
}

fn parse_ptp2(d: &[u8]) -> Value {
    let mtype = d[0] & 0x0F;
    let mut m = Map::new();
    m.insert("version".into(), json!(2));
    m.insert("message_type".into(), json!(mtype));
    m.insert("message_name".into(), json!(ptp2_type(mtype)));
    m.insert("domain".into(), json!(d[4]));
    m.insert("clock_identity".into(), json!(fmt_id(&d[20..28])));
    m.insert("port_number".into(), json!(be16(d, 28)));
    m.insert("sequence_id".into(), json!(be16(d, 30)));
    m.insert("log_message_interval".into(), json!(d[33] as i8));
    if mtype == 0xB && d.len() >= 64 {
        let mut a = Map::new();
        a.insert("current_utc_offset".into(), json!(be16(d, 44) as i16));
        a.insert("priority1".into(), json!(d[47]));
        a.insert("clock_class".into(), json!(d[48]));
        a.insert("clock_accuracy".into(), json!(format!("0x{:02X}", d[49])));
        a.insert("offset_scaled_log_variance".into(), json!(format!("0x{:04X}", be16(d, 50))));
        a.insert("priority2".into(), json!(d[52]));
        a.insert("grandmaster_identity".into(), json!(fmt_id(&d[53..61])));
        a.insert("steps_removed".into(), json!(be16(d, 61)));
        a.insert("time_source".into(), json!(time_source(d[63])));
        m.insert("announce".into(), Value::Object(a));
    }
    Value::Object(m)
}

fn parse_ptp1(d: &[u8]) -> Value {
    let control = d[32];
    let mut m = Map::new();
    m.insert("version".into(), json!(1));
    m.insert("version_network".into(), json!(be16(d, 2)));
    m.insert("subdomain".into(), json!(nul_str(&d[4..20])));
    m.insert("message_type".into(), json!(d[20]));
    m.insert("control".into(), json!(control));
    m.insert("message_name".into(), json!(ptp1_control(control)));
    m.insert("clock_identity".into(), json!(fmt_id(&d[22..28])));
    m.insert("port_number".into(), json!(be16(d, 28)));
    m.insert("sequence_id".into(), json!(be16(d, 30)));
    // Sync / Delay_Req bodies carry the grandmaster dataset (124-byte message)
    if (control == 0 || control == 1) && d.len() >= 124 {
        let mut s = Map::new();
        s.insert("grandmaster_identity".into(), json!(fmt_id(&d[54..60])));
        s.insert("grandmaster_port".into(), json!(be16(d, 60)));
        s.insert("grandmaster_stratum".into(), json!(d[67]));
        s.insert("grandmaster_identifier".into(), json!(nul_str(&d[68..72])));
        s.insert("grandmaster_variance".into(), json!(be16(d, 74) as i16));
        s.insert("grandmaster_preferred".into(), json!(d[77] != 0));
        s.insert("grandmaster_is_boundary_clock".into(), json!(d[79] != 0));
        s.insert("sync_interval".into(), json!(d[83] as i8));
        s.insert("local_clock_variance".into(), json!(be16(d, 86) as i16));
        s.insert("steps_removed".into(), json!(be16(d, 90)));
        s.insert("local_clock_stratum".into(), json!(d[95]));
        s.insert("local_clock_identifier".into(), json!(nul_str(&d[96..100])));
        s.insert("parent_uuid".into(), json!(fmt_id(&d[102..108])));
        m.insert("sync".into(), Value::Object(s));
    }
    Value::Object(m)
}

// ---------------------------------------------------------------------------
// PTP, passive: read-only MQTT subscription to the apk-ptp discovery topic
// ---------------------------------------------------------------------------

fn mqtt_str(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut v = (b.len() as u16).to_be_bytes().to_vec();
    v.extend_from_slice(b);
    v
}

fn mqtt_packet(ptype: u8, body: &[u8]) -> Vec<u8> {
    let mut n = body.len();
    let mut out = vec![ptype];
    loop {
        let d = (n % 128) as u8;
        n /= 128;
        out.push(d | if n != 0 { 0x80 } else { 0 });
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(body);
    out
}

enum MqttErr {
    Timeout,
    Fail(String),
}

impl From<io::Error> for MqttErr {
    fn from(e: io::Error) -> Self {
        if py::is_timeout(&e) { MqttErr::Timeout } else { MqttErr::Fail(py::os_error(&e)) }
    }
}

/// Read one MQTT packet -> (type byte, body). A timeout before the first byte is
/// reported as Timeout; once a packet has started, it is read to the end.
fn mqtt_read(sock: &mut TcpStream, stop: &AtomicBool) -> Result<(u8, Vec<u8>), MqttErr> {
    fn need(sock: &mut TcpStream, k: usize, started: bool, stop: &AtomicBool) -> Result<Vec<u8>, MqttErr> {
        let mut buf = vec![0u8; k];
        let mut got = 0;
        while got < k {
            match sock.read(&mut buf[got..]) {
                Ok(0) => return Err(MqttErr::Fail("broker closed the connection".into())),
                Ok(n) => got += n,
                Err(e) if py::is_timeout(&e) => {
                    if !started && got == 0 {
                        return Err(MqttErr::Timeout);
                    }
                    if stop.load(Ordering::Relaxed) {
                        return Err(MqttErr::Fail("stopped".into()));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(buf)
    }
    let head = need(sock, 1, false, stop)?[0];
    let (mut mult, mut length) = (1usize, 0usize);
    loop {
        let d = need(sock, 1, true, stop)?[0];
        length += (d & 0x7F) as usize * mult;
        if d & 0x80 == 0 {
            break;
        }
        mult *= 128;
        if mult > 128 * 128 * 128 * 128 {
            return Err(MqttErr::Fail("malformed remaining length".into()));
        }
    }
    let body = if length > 0 { need(sock, length, true, stop)? } else { vec![] };
    Ok((head, body))
}

/// Map apk-ptp discovery rows onto the clocks contract. Err where the Python raises
/// (a row that is not an object, a non-string grandmaster).
pub fn parse_ptp_discovery(rows: &Value) -> Result<Vec<Value>, String> {
    let num = |v: Option<&Value>| -> Value {
        let s = py::str_of(v.unwrap_or(&Value::Null));
        py::parse_int(&s).map(|n| json!(n)).unwrap_or(Value::Null)
    };
    let none_if_dash = |v: Option<&Value>| -> Value {
        match v {
            None | Some(Value::Null) => Value::Null,
            Some(Value::String(s)) if s == "-" => Value::Null,
            Some(x) => x.clone(),
        }
    };
    let or_none = |v: Option<&Value>| -> Value { v.filter(|x| py::truthy(x)).cloned().unwrap_or(Value::Null) };
    let mut out = Vec::new();
    let list = match rows.as_array() {
        Some(l) => l,
        None => return Ok(out),
    };
    for r in list {
        let r = r.as_object().ok_or("'row' object has no attribute 'get'")?;
        let variant = r.get("variant").map(py::str_of).unwrap_or_default();
        let vendor_v = r
            .get("vendor")
            .filter(|v| py::truthy(v))
            .or_else(|| r.get("manufacturer").filter(|v| py::truthy(v)));
        let vendor = vendor_v.map(py::str_of).unwrap_or_default();
        let vlow = variant.to_lowercase();
        let v1 = vlow.contains("v1");
        let sub = r.get("subdomain");
        let sub_ok = matches!(sub, Some(v) if !v.is_null() && v != &json!("-"));
        let domain = if v1 && sub_ok { sub.cloned().unwrap_or(Value::Null) } else { num(r.get("domain")) };
        let vl = vendor.to_lowercase();
        let network = if vlow == "gptp" {
            "avb (gPTP)"
        } else if v1 || ["presonus", "audinate", "focusrite", "yamaha"].iter().any(|k| vl.contains(k)) {
            "dante"
        } else if ["bridgeco", "archwave", "merging", "lawo", "alc networx", "digigram"].iter().any(|k| vl.contains(k)) {
            "ravenna/aes67"
        } else {
            "aes67"
        };
        let mut msgs: BTreeMap<String, i64> = BTreeMap::new();
        let mtext = r.get("messages").map(py::str_of).unwrap_or_default();
        for m in mtext.split(',') {
            if !m.contains('\u{00d7}') {
                continue;
            }
            let mut sp = m.split('\u{00d7}');
            let k = py::strip(sp.next().unwrap_or("")).to_string();
            let n = py::strip(sp.next().unwrap_or(""));
            if py::is_digits(n) {
                if let Ok(v) = n.parse::<i64>() {
                    msgs.insert(k, v);
                }
            }
        }
        let gm = match r.get("grandmaster") {
            Some(Value::String(s)) if s.is_empty() || s == "-" => Value::Null,
            Some(Value::String(s)) => json!(s.replace(':', "-")),
            Some(v) if py::truthy(v) => return Err("'grandmaster' has no attribute 'replace'".into()),
            _ => Value::Null,
        };
        let cid = r.get("clock_id").map(py::str_of).unwrap_or_default().replace(':', "-");
        let mget = |k: &str| msgs.get(k).copied().unwrap_or(0);
        let mut c = Map::new();
        c.insert("clock_identity".into(), json!(cid));
        c.insert("version".into(), json!(if v1 { 1 } else { 2 }));
        c.insert("variant".into(), json!(variant));
        c.insert("domain".into(), domain);
        c.insert("source_ip".into(), or_none(r.get("ip")));
        c.insert("network".into(), json!(network));
        c.insert("vendor".into(), if vendor.is_empty() { Value::Null } else { json!(vendor) });
        c.insert("grandmaster_identity".into(), gm);
        c.insert("is_grandmaster".into(), json!(r.get("role") == Some(&json!("grandmaster"))));
        c.insert("priority1".into(), num(r.get("priority1")));
        c.insert("priority2".into(), num(r.get("priority2")));
        c.insert("clock_class".into(), num(r.get("gm_class")));
        c.insert("clock_accuracy".into(), none_if_dash(r.get("gm_accuracy")));
        c.insert("steps_removed".into(), num(r.get("steps_removed")));
        c.insert("time_source".into(), none_if_dash(r.get("time_source")));
        c.insert("sync_health".into(), r.get("sync_health").cloned().unwrap_or(Value::Null));
        let online = r.get("_row_state") == Some(&json!("online")) || r.get("_presence") == Some(&json!("online"));
        c.insert("online".into(), json!(online));
        c.insert("sync_count".into(), json!(mget("Sync")));
        c.insert("follow_up_count".into(), json!(mget("Follow_Up")));
        c.insert("delay_req_count".into(), json!(mget("Delay_Req") + mget("Pdelay_Req")));
        c.insert("announce_count".into(), json!(mget("Announce")));
        c.insert("last_seen_text".into(), r.get("last_seen").cloned().unwrap_or(Value::Null));
        // PTP minus this host's (NTP-disciplined) clock, from the sniffer (built-in only).
        c.insert("ptp_offset".into(), r.get("ptp_offset").filter(|v| v.is_object()).cloned().unwrap_or(Value::Null));
        // The NIC the clock was heard on (built-in sniffer only).
        c.insert("interface".into(), r.get("interface").filter(|v| py::truthy(v)).cloned().unwrap_or(Value::Null));
        out.push(Value::Object(c));
    }
    Ok(out)
}

fn mqtt_connect(host: &str, port: u16) -> Result<TcpStream, String> {
    let addrs: Vec<SocketAddr> = (host, port).to_socket_addrs().map_err(|e| py::os_error(&e))?.collect();
    let mut last = "getaddrinfo returns an empty list".to_string();
    for a in addrs {
        match TcpStream::connect_timeout(&a, Duration::from_secs(5)) {
            Ok(s) => return Ok(s),
            Err(e) => last = py::os_error(&e),
        }
    }
    Err(last)
}

fn mqtt_session(stop: &AtomicBool, backoff: &mut f64) -> Result<(), String> {
    let cfg = config();
    let mut sock = mqtt_connect(&cfg.mqtt_host, cfg.mqtt_port)?;
    let io_err = |e: io::Error| py::os_error(&e);
    sock.set_read_timeout(Some(Duration::from_secs(5))).map_err(io_err)?;
    sock.set_write_timeout(Some(Duration::from_secs(5))).map_err(io_err)?;
    let cid = format!("wbs-aoip-ptp-{}", std::process::id());
    let mut body = mqtt_str("MQTT");
    body.extend_from_slice(&[4, 0x02]);
    body.extend_from_slice(&60u16.to_be_bytes());
    body.extend(mqtt_str(&cid));
    sock.write_all(&mqtt_packet(0x10, &body)).map_err(io_err)?; // CONNECT, clean session
    let (head, ack) = match mqtt_read(&mut sock, stop) {
        Ok(p) => p,
        Err(MqttErr::Timeout) => return Err("timed out".into()),
        Err(MqttErr::Fail(s)) => return Err(s),
    };
    if head >> 4 != 2 || ack.len() < 2 || ack[1] != 0 {
        let code = if ack.len() > 1 { ack[1].to_string() } else { "?".into() };
        return Err(format!("CONNACK refused ({code})"));
    }
    let mut sub = 1u16.to_be_bytes().to_vec();
    sub.extend(mqtt_str(&cfg.ptp_topic));
    sub.push(0);
    sock.write_all(&mqtt_packet(0x82, &sub)).map_err(io_err)?;
    {
        let mut r = registry();
        r.mqtt.connected = true;
        r.mqtt.error = None;
    }
    *backoff = 1.0;
    let mut last_ping = Instant::now();
    sock.set_read_timeout(Some(Duration::from_secs(1))).map_err(io_err)?;
    while !stop.load(Ordering::Relaxed) {
        if last_ping.elapsed() > Duration::from_secs(30) {
            sock.write_all(b"\xc0\x00").map_err(io_err)?; // PINGREQ
            last_ping = Instant::now();
        }
        let (head, body) = match mqtt_read(&mut sock, stop) {
            Ok(p) => p,
            Err(MqttErr::Timeout) => continue,
            Err(MqttErr::Fail(s)) => return Err(s),
        };
        if head >> 4 != 3 {
            continue; // only PUBLISH matters
        }
        if body.len() < 2 {
            return Err("unpack requires a buffer of 2 bytes".into());
        }
        let tlen = be16(&body, 0) as usize;
        let off = 2 + tlen + if (head >> 1) & 3 != 0 { 2 } else { 0 };
        let payload = body.get(off..).unwrap_or(&[]);
        let rows = match std::str::from_utf8(payload)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(t).ok())
            .and_then(|v| parse_ptp_discovery(&v).ok())
        {
            Some(r) => r,
            None => continue,
        };
        let mut r = registry();
        r.mqtt_rows = rows;
        r.mqtt.messages += 1;
        r.mqtt.updated = Some(now_f64());
    }
    Ok(())
}

/// Subscribe (QoS 0, read-only) and keep the latest retained discovery list.
fn run_ptp_mqtt(stop: Arc<AtomicBool>) {
    let cfg = config();
    let mut backoff = 1.0f64;
    while !stop.load(Ordering::Relaxed) {
        if let Err(e) = mqtt_session(&stop, &mut backoff) {
            let mut r = registry();
            r.mqtt.connected = false;
            r.mqtt.error = Some(format!("MQTT {}:{} {}: {}", cfg.mqtt_host, cfg.mqtt_port, cfg.ptp_topic, e));
        }
        py::wait(&stop, backoff);
        backoff = (backoff * 2.0).min(30.0);
    }
}

// ---------------------------------------------------------------------------
// PTP: the APK sniffer (apkaudio-ptp), built in
// ---------------------------------------------------------------------------

/// Clocks not heard for this long leave the table (apk-ptp's clock_timeout_s).
const APK_CLOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// apk-ptp's snapshot_json clocks -> the discovery rows parse_ptp_discovery reads.
pub fn apk_rows(snapshot: &Value, vendor: &dyn Fn(&str) -> Option<String>) -> Value {
    let rows: Vec<Value> = snapshot["clocks"]
        .as_array()
        .map(|cs| {
            cs.iter()
                .map(|c| {
                    let id = c["clock_id"].as_str().unwrap_or("");
                    let src = c["source"].as_str().unwrap_or("");
                    // UDP clocks carry the sender IP; gPTP (Layer 2) only a MAC.
                    let ip = src.parse::<Ipv4Addr>().ok().map(|_| json!(src)).unwrap_or(Value::Null);
                    json!({
                        "clock_id": id,
                        "variant": c["variant"],
                        "domain": c["domain"],
                        "subdomain": c["subdomain"],
                        "vendor": vendor(id),
                        "ip": ip,
                        "grandmaster": c["grandmaster"].as_str().unwrap_or("-"),
                        "role": c["role"],
                        "priority1": c["priority1"],
                        "priority2": c["priority2"],
                        "gm_class": c["gm_class"],
                        "gm_accuracy": c["gm_accuracy"],
                        "steps_removed": c["steps_removed"],
                        "time_source": c["time_source"],
                        "sync_health": c["diagnostics"]["verdict"],
                        "messages": c["messages"],
                        "interface": c["interface"],
                        "ptp_offset": ptp_offset(&c["diagnostics"]),
                        "_row_state": "online",
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Value::Array(rows)
}

/// PTP time minus this host's clock (which NTP sets), from the sniffer's apparent
/// offset: that is (host - master) plus a few us of network and capture delay, so
/// PTP - NTP is its negation. Null until a Sync/Follow_Up pair has been timed.
pub fn ptp_offset(diag: &Value) -> Value {
    let Some(apparent) = diag["apparent_offset_ns"].as_f64() else {
        return Value::Null;
    };
    json!({
        "ptp_minus_ntp_ns": -apparent,
        "spread_ns": diag["offset_spread_ns"],
        "drift_ns_per_s": diag["drift_ns_per_s"],
        "samples": diag["offset_samples"],
        "ptp_timescale": diag["ptp_timescale"],
        "utc_offset_applied": diag["utc_offset_applied"],
    })
}

/// What changed between two clock lists: clocks heard / gone, leaders elected,
/// contested / settled, class or priority changes. Each event: {kind, text, port, domain}.
pub fn ptp_events(prev: &[Value], now: &[Value]) -> Vec<Value> {
    let key = |c: &Value| {
        format!("{}|{}|{}|{}", c["interface"].as_str().unwrap_or(""), c["version"], py::str_of(&c["domain"]), c["clock_identity"].as_str().unwrap_or(""))
    };
    let name = |c: &Value| {
        let port = c["interface"].as_str().map(|p| format!(" on {p}")).unwrap_or_default();
        format!("PTPv{} domain {}{port}", c["version"], py::str_of(&c["domain"]))
    };
    let ev = |kind: &str, text: String, c: &Value| {
        json!({"kind": kind, "text": text, "port": c["interface"], "domain": c["domain"], "version": c["version"]})
    };
    let old: std::collections::HashMap<String, &Value> = prev.iter().map(|c| (key(c), c)).collect();
    let new: std::collections::HashMap<String, &Value> = now.iter().map(|c| (key(c), c)).collect();
    let mut out = Vec::new();
    for c in now {
        let id = c["clock_identity"].as_str().unwrap_or("");
        match old.get(&key(c)) {
            None => {
                out.push(ev("heard", format!("{id} heard ({}, {})", name(c), c["network"].as_str().unwrap_or("")), c));
                if c["is_grandmaster"] == true {
                    out.push(ev("leader", format!("{id} leads {}", name(c)), c));
                }
            }
            Some(p) => {
                if c["is_grandmaster"] == true && p["is_grandmaster"] != true {
                    out.push(ev("leader", format!("{id} became leader of {}", name(c)), c));
                }
                if c["is_grandmaster"] != true && p["is_grandmaster"] == true {
                    out.push(ev("follower", format!("{id} stopped leading {}", name(c)), c));
                }
                if c["sync_health"] == "contested" && p["sync_health"] != "contested" {
                    out.push(ev("contested", format!("{id}: {} is contested (two leaders)", name(c)), c));
                }
                if p["sync_health"] == "contested" && c["sync_health"] != "contested" {
                    out.push(ev("settled", format!("{id}: {} settled", name(c)), c));
                }
                for (k, label) in [("priority1", "priority1"), ("clock_class", "class")] {
                    if !p[k].is_null() && !c[k].is_null() && p[k] != c[k] {
                        out.push(ev("changed", format!("{id} {label} {} → {}", p[k], c[k]), c));
                    }
                }
            }
        }
    }
    for c in prev {
        if !new.contains_key(&key(c)) {
            out.push(ev("gone", format!("{} gone ({})", c["clock_identity"].as_str().unwrap_or(""), name(c)), c));
        }
    }
    out
}

/// Most recent PTP events kept for the Timing tab.
const PTP_EVENTS_MAX: usize = 200;

/// The runtime prompt for a sniffer that can't open its sockets.
pub fn apk_fix(problems: &[String], udp_blocked: bool, exe: &str) -> Option<Value> {
    let perm = problems.iter().any(|p| {
        let l = p.to_lowercase();
        l.contains("permission") || l.contains("not permitted") || l.contains("cap_net")
    });
    if perm {
        return Some(json!({
            "title": "Let the TAP listen for PTP clocks",
            "why": "PTP uses UDP ports 319/320 and gPTP raw Ethernet, which need two capabilities on the TAP program. \
                    Granting them needs your password, so run this in a terminal.",
            "options": [
                {"label": "Grant and restart", "note": "kept until the TAP is reinstalled (install.sh grants it again)",
                 "command": format!("sudo setcap cap_net_raw,cap_net_bind_service+ep {exe} && systemctl --user restart ward-beck-audio-tab")},
            ],
        }));
    }
    if udp_blocked {
        return Some(json!({
            "title": "Allow PTP through the firewall",
            "why": "gPTP arrives on raw Ethernet but nothing reaches UDP 319/320: the host firewall is dropping PTPv1/PTPv2.",
            "options": [
                {"label": "This session only", "note": "gone at the next reboot",
                 "command": "sudo iptables -I INPUT -p udp -d 224.0.1.129 --dport 319:320 -j ACCEPT"},
                {"label": "Permanently", "note": "saved as a UFW rule",
                 "command": "sudo ufw allow proto udp to 224.0.1.129 port 319:320"},
            ],
        }));
    }
    None
}

/// One short line for the status bar; the long how-to lives in the fix prompt.
fn apk_error(problems: &[String], has_fix: bool) -> Option<String> {
    if problems.is_empty() {
        None
    } else if has_fix {
        Some("no permission to listen for PTP (UDP 319/320, raw Ethernet)".into())
    } else {
        Some(problems.iter().map(|p| p.lines().next().unwrap_or("")).collect::<Vec<_>>().join("; "))
    }
}

fn run_ptp_apk(stop: Arc<AtomicBool>) {
    use apkaudio_ptp::diagnostics::Diagnostics;
    use apkaudio_ptp::flow::FlowTracker;
    use apkaudio_ptp::monitor::{self, ClockTable, Observation, Transports};
    use apkaudio_ptp::report::{snapshot_json, Counters};

    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "ward-beck-audio-tab".into());
    #[cfg(feature = "vendors")]
    let vendors = apkaudio_maclookup::MacVendors::start();
    #[cfg(feature = "vendors")]
    let vendor = |id: &str| vendors.vendor(id);
    #[cfg(not(feature = "vendors"))]
    let vendor = |_: &str| -> Option<String> { None };
    while !stop.load(Ordering::Relaxed) {
        let transports = Transports::open();
        let open = transports.any_open();
        {
            let mut r = registry();
            r.mqtt.connected = open;
            r.mqtt.fix = apk_fix(&transports.problems, false, &exe);
            r.mqtt.error = apk_error(&transports.problems, r.mqtt.fix.is_some());
        }
        if !open {
            // Nothing to listen with; try again later (a setcap + restart is the usual cure).
            py::wait(&stop, 10.0);
            continue;
        }
        let mut tracker = FlowTracker::default();
        let mut table = ClockTable::default();
        let mut diagnostics = Diagnostics::default();
        let mut counts = Counters::default();
        let mut last = Instant::now();
        let started = Instant::now();
        monitor::run_paced(
            &transports,
            &mut tracker,
            |obs| {
                if let Some(obs) = obs {
                    counts.total += 1;
                    match obs {
                        Observation::V2(msg, _, meta) => {
                            if meta.port.is_some() { counts.udp += 1 } else { counts.l2 += 1 }
                            table.record(&msg, &meta);
                            diagnostics.observe(&msg, &meta);
                        }
                        Observation::V1(msg, meta) => {
                            counts.udp += 1;
                            table.record_v1(&msg, &meta);
                        }
                        Observation::Undecodable { .. } => counts.undecodable += 1,
                    }
                    return;
                }
                if last.elapsed() < Duration::from_secs(1) {
                    return;
                }
                last = Instant::now();
                table.clocks.retain(|_, c| c.last_seen.elapsed() < APK_CLOCK_TIMEOUT);
                let snap: Value = serde_json::from_str(&snapshot_json(
                    &table,
                    &diagnostics,
                    &transports,
                    &counts,
                    started.elapsed().as_secs_f64(),
                ))
                .unwrap_or(Value::Null);
                let blocked = snap["counts"]["udp_blocked"].as_bool().unwrap_or(false);
                let rows = parse_ptp_discovery(&apk_rows(&snap, &vendor)).unwrap_or_default();
                let mut r = registry();
                let at = now_f64();
                for mut e in ptp_events(&r.mqtt_rows, &rows) {
                    e["t"] = json!(at);
                    r.mqtt.events.push_back(e);
                }
                while r.mqtt.events.len() > PTP_EVENTS_MAX {
                    r.mqtt.events.pop_front();
                }
                r.mqtt_rows = rows;
                r.mqtt.messages = counts.total;
                r.mqtt.updated = Some(now_f64());
                r.mqtt.fix = apk_fix(&transports.problems, blocked, &exe);
            },
            || stop.load(Ordering::Relaxed),
        );
    }
}

// ---------------------------------------------------------------------------
// SIP
// ---------------------------------------------------------------------------

pub fn parse_sip(data: &[u8]) -> Result<Value, String> {
    let text = String::from_utf8_lossy(data);
    let head = text.split("\r\n\r\n").next().unwrap_or("");
    let head = head.split("\n\n").next().unwrap_or("");
    let lines: Vec<&str> = head.split('\n').map(|l| l.trim_end_matches('\r')).collect();
    if lines.is_empty() || !lines[0].contains("SIP/2.0") {
        return Err("not a SIP message".into());
    }
    let first = py::strip(lines[0]);
    let parts: Vec<&str> = first.splitn(3, ' ').collect();
    let (method, is_request) = if parts[0].to_uppercase().starts_with("SIP/") {
        let m = if parts.len() > 2 {
            format!("{} {}", parts[1], parts[2])
        } else if parts.len() > 1 {
            parts[1].to_string()
        } else {
            first.to_string()
        };
        (m, false)
    } else {
        (parts[0].to_uppercase(), true)
    };
    let mut hdrs: BTreeMap<String, String> = BTreeMap::new();
    for l in &lines[1..] {
        let (k, v) = match l.split_once(':') {
            Some(kv) => kv,
            None => continue,
        };
        let k = py::strip(k).to_lowercase();
        let k = match k.as_str() {
            "f" => "from".to_string(),
            "t" => "to".to_string(),
            "i" => "call-id".to_string(),
            "m" => "contact".to_string(),
            _ => k,
        };
        hdrs.entry(k).or_insert_with(|| py::strip(v).to_string());
    }
    let h = |k: &str| hdrs.get(k).map(|v| json!(v)).unwrap_or(Value::Null);
    let ua = match hdrs.get("user-agent") {
        Some(v) if !v.is_empty() => json!(v),
        _ => h("server"),
    };
    let mut m = Map::new();
    m.insert("method".into(), json!(method));
    m.insert("is_request".into(), json!(is_request));
    m.insert("start_line".into(), json!(first));
    m.insert("from".into(), h("from"));
    m.insert("to".into(), h("to"));
    m.insert("call_id".into(), h("call-id"));
    m.insert("contact".into(), h("contact"));
    m.insert("user_agent".into(), ua);
    m.insert("cseq".into(), h("cseq"));
    Ok(Value::Object(m))
}

// ---------------------------------------------------------------------------
// Sockets & threads
// ---------------------------------------------------------------------------

struct OpenErr {
    class: &'static str,
    msg: String,
    errno: Option<i32>,
}

impl From<io::Error> for OpenErr {
    fn from(e: io::Error) -> Self {
        OpenErr { class: py::exc_class(&e), msg: py::os_error(&e), errno: e.raw_os_error() }
    }
}

fn describe_error(e: &OpenErr, port: u16) -> String {
    if e.class == "PermissionError" || matches!(e.errno, Some(1) | Some(13)) {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| std::fs::canonicalize(&p).ok().or(Some(p)))
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        return format!(
            "bind UDP {port}: {}; needs CAP_NET_BIND_SERVICE (sudo setcap cap_net_bind_service=+ep {exe}) or run as root",
            e.msg
        );
    }
    format!("UDP {port}: {}: {}", e.class, e.msg)
}

fn parse_v4(s: &str) -> Result<Ipv4Addr, OpenErr> {
    s.parse().map_err(|_| OpenErr { class: "OSError", msg: "illegal IP address string passed to inet_aton".into(), errno: None })
}

/// Bind UDP `port` and join each multicast group. Returns (sock, joined_ifaces, join_errors).
fn open_socket(groups: &[&str], port: u16) -> Result<(UdpSocket, Vec<String>, Vec<String>), OpenErr> {
    use socket2::{Domain, InterfaceIndexOrAddress, Protocol, SockAddr, Socket, Type};
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    crate::sys::reuse_port(&sock);
    sock.bind(&SockAddr::from(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))))?;
    let ifc = iface();
    let mut joined = Vec::new();
    let mut errors = Vec::new();
    if ifc != "0.0.0.0" {
        let local = parse_v4(&ifc)?;
        for g in groups {
            sock.join_multicast_v4(&parse_v4(g)?, &local)?;
        }
        joined.push(ifc);
    } else {
        for (idx, name) in py::if_nameindex() {
            let mut ok = false;
            for g in groups {
                // struct ip_mreqn {multiaddr, address, ifindex} (Linux)
                match sock.join_multicast_v4_n(&parse_v4(g)?, &InterfaceIndexOrAddress::Index(idx)) {
                    Ok(()) => ok = true,
                    Err(e) => errors.push(format!("{name}: {}", py::os_error(&e))),
                }
            }
            if ok {
                joined.push(name);
            }
        }
        if joined.is_empty() {
            for g in groups {
                sock.join_multicast_v4(&parse_v4(g)?, &Ipv4Addr::UNSPECIFIED)?;
            }
            joined.push("INADDR_ANY".into());
        }
    }
    sock.set_read_timeout(Some(Duration::from_secs(1)))?;
    Ok((sock.into(), joined, errors))
}

#[derive(Clone, Copy)]
enum Kind {
    Sap,
    Sip,
    Ptp(u16),
}

fn rx_loop(sock: UdpSocket, kind: Kind, stop: &AtomicBool) {
    let mut buf = vec![0u8; 65535];
    while !stop.load(Ordering::Relaxed) {
        let (n, addr) = match sock.recv_from(&mut buf) {
            Ok(r) => r,
            Err(e) if py::is_timeout(&e) => continue,
            Err(_) => {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        let ip = addr.ip().to_string();
        let data = &buf[..n];
        let now = now_f64();
        let mut r = registry();
        match kind {
            Kind::Sap => {
                r.sap.packets += 1;
                r.handle_sap(data, &ip, now);
            }
            Kind::Sip => {
                r.sip.packets += 1;
                r.handle_sip(data, &ip, now);
            }
            Kind::Ptp(_) => {
                r.ptp.packets += 1;
                r.handle_ptp(data, &ip, now);
            }
        }
    }
}

fn run_listener(groups: Vec<&'static str>, port: u16, kind: Kind, stop: Arc<AtomicBool>) {
    let (sock, joined, join_errors) = match open_socket(&groups, port) {
        Ok(v) => v,
        Err(e) => {
            let mut r = registry();
            let msg = describe_error(&e, port);
            match kind {
                Kind::Ptp(p) => {
                    r.ptp_errors.insert(p, msg);
                    let joined = r.ptp_errors.values().cloned().collect::<Vec<_>>().join("; ");
                    r.ptp.error = Some(joined);
                }
                Kind::Sap => {
                    r.sap.listening = false;
                    r.sap.error = Some(msg);
                }
                Kind::Sip => {
                    r.sip.listening = false;
                    r.sip.error = Some(msg);
                }
            }
            return;
        }
    };
    {
        let mut r = registry();
        let r = &mut *r;
        let st = match kind {
            Kind::Sap => &mut r.sap,
            Kind::Sip => &mut r.sip,
            Kind::Ptp(_) => &mut r.ptp,
        };
        st.listening = true;
        for j in joined {
            if !st.joined_interfaces.contains(&j) {
                st.joined_interfaces.push(j);
            }
        }
        st.join_errors.extend(join_errors.into_iter().take(20));
        if let Kind::Ptp(p) = kind {
            r.ptp_bound.insert(p);
        }
    }
    rx_loop(sock, kind, &stop);
}

/// Start SAP + SIP listeners and the PTP source (passive MQTT by default) (idempotent).
pub fn start(stop: Arc<AtomicBool>) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let mut specs: Vec<(&str, Vec<&'static str>, u16, Kind)> = vec![
        ("aoip-sap", SAP_GROUPS.iter().map(|(g, _)| *g).collect(), 9875, Kind::Sap),
        ("aoip-sip", vec![SIP_GROUP.0], SIP_GROUP.1, Kind::Sip),
    ];
    if sockets_mode() {
        specs.push(("aoip-ptp-319", vec![PTP_GROUP], 319, Kind::Ptp(319)));
        specs.push(("aoip-ptp-320", vec![PTP_GROUP], 320, Kind::Ptp(320)));
    }
    for (name, groups, port, kind) in specs {
        let stop = stop.clone();
        let _ = std::thread::Builder::new().name(name.into()).spawn(move || run_listener(groups, port, kind, stop));
    }
    if apk_mode() {
        let _ = std::thread::Builder::new().name("aoip-ptp-apk".into()).spawn(move || run_ptp_apk(stop));
    } else if !sockets_mode() {
        let stop = stop.clone();
        let _ = std::thread::Builder::new().name("aoip-ptp-mqtt".into()).spawn(move || run_ptp_mqtt(stop));
    }
}

/// The PTP leader an AES67 sender should name: a PTPv2 grandmaster (not gPTP, not
/// Dante PTPv1) on the NIC the streams leave from, AES67/RAVENNA networks first,
/// then the best priority1, then domain.
pub fn pick_ptp_leader(gms: &Value, tx_iface: Option<&str>) -> Option<(String, i64)> {
    let mut v2: Vec<&Value> = gms
        .as_array()?
        .iter()
        .filter(|g| g["version"].as_i64() == Some(2) && g["domain"].is_i64() && g["network"] != "avb (gPTP)")
        .filter(|g| g["identity"].as_str().is_some_and(|s| !s.is_empty()))
        .collect();
    v2.sort_by_key(|g| {
        (
            tx_iface.is_some() && g["interface"].as_str() != tx_iface,
            !g["network"].as_str().unwrap_or("").contains("aes67"),
            g["priority1"].as_i64().unwrap_or(255),
            g["domain"].as_i64().unwrap_or(0),
        )
    });
    v2.first().map(|g| (g["identity"].as_str().unwrap_or("").to_string(), g["domain"].as_i64().unwrap_or(0)))
}

/// The PTP leader heard right now, if any.
pub fn ptp_leader() -> Option<(String, i64)> {
    let host = crate::aes67::host_v4();
    let iface = apkaudio_ptp::net::list_interfaces()
        .unwrap_or_default()
        .into_iter()
        .find(|(_, _, _, ip)| *ip == host)
        .map(|(name, ..)| name);
    pick_ptp_leader(&snapshot()["ptp"]["grandmasters"], iface.as_deref())
}

/// Thread-safe JSON-ready view of everything heard on the multicast groups.
pub fn snapshot() -> Value {
    let now = now_f64();
    let dante = dante_ips();
    registry().snapshot_at(now, &dante)
}

// ---------------------------------------------------------------------------
// Tests (port of test_aoip_listener.py: hand-built packets, no network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const GM: [u8; 8] = [0x00, 0x1D, 0xC1, 0xFF, 0xFE, 0x12, 0x34, 0x56];
    const GM_STR: &str = "00-1D-C1-FF-FE-12-34-56";

    const SDP: &str = concat!(
        "v=0\r\n",
        "o=- 1311738121 1311738121 IN IP4 192.168.1.50\r\n",
        "s=Stage Box 1-2\r\n",
        "c=IN IP4 239.69.1.10/32\r\n",
        "t=0 0\r\n",
        "a=ts-refclk:ptp=IEEE1588-2008:00-1D-C1-FF-FE-12-34-56:0\r\n",
        "a=mediaclk:direct=0\r\n",
        "m=audio 5004 RTP/AVP 96\r\n",
        "a=rtpmap:96 L24/48000/2\r\n",
        "a=ptime:1\r\n",
    );

    fn sap_packet(sdp: &str, deletion: bool, msg_hash: u16, origin: [u8; 4], ptype: bool) -> Vec<u8> {
        let b0 = (1u8 << 5) | if deletion { 0x04 } else { 0 };
        let mut p = vec![b0, 0];
        p.extend_from_slice(&msg_hash.to_be_bytes());
        p.extend_from_slice(&origin);
        if ptype {
            p.extend_from_slice(b"application/sdp\x00");
        }
        p.extend_from_slice(sdp.as_bytes());
        p
    }

    fn sap(sdp: &str) -> Vec<u8> {
        sap_packet(sdp, false, 0xBEEF, [192, 168, 1, 50], true)
    }

    fn ptp2_announce(clock: [u8; 8], gm: [u8; 8], domain: u8) -> Vec<u8> {
        let mut h = vec![0u8; 34];
        h[0] = 0x0B;
        h[1] = 0x02;
        h[2..4].copy_from_slice(&64u16.to_be_bytes());
        h[4] = domain;
        h[20..28].copy_from_slice(&clock);
        h[28..30].copy_from_slice(&1u16.to_be_bytes());
        h[30..32].copy_from_slice(&42u16.to_be_bytes());
        h[32] = 5;
        h[33] = 1;
        let mut b = vec![0u8; 30];
        b[10..12].copy_from_slice(&37i16.to_be_bytes()); // currentUtcOffset (offset 44)
        b[13] = 128; // priority1 (47)
        b[14] = 248; // clockClass (48)
        b[15] = 0x20; // clockAccuracy (49)
        b[16..18].copy_from_slice(&0x4E5Du16.to_be_bytes()); // offsetScaledLogVariance (50)
        b[18] = 127; // priority2 (52)
        b[19..27].copy_from_slice(&gm); // grandmasterIdentity (53)
        b[27..29].copy_from_slice(&0u16.to_be_bytes()); // stepsRemoved (61)
        b[29] = 0xA0; // timeSource (63)
        h.extend(b);
        h
    }

    fn ptp1_sync() -> Vec<u8> {
        let uuid = [0x00, 0x1D, 0xC1, 0x12, 0x34, 0x56];
        let mut p = vec![0u8; 124];
        p[0..2].copy_from_slice(&1u16.to_be_bytes());
        p[2..4].copy_from_slice(&1u16.to_be_bytes());
        p[4..9].copy_from_slice(b"_DFLT");
        p[20] = 1; // event message
        p[21] = 1; // ethernet
        p[22..28].copy_from_slice(&uuid);
        p[28..30].copy_from_slice(&1u16.to_be_bytes());
        p[30..32].copy_from_slice(&7u16.to_be_bytes());
        p[32] = 0; // control = Sync
        p[53] = 1;
        p[54..60].copy_from_slice(&uuid);
        p[60..62].copy_from_slice(&1u16.to_be_bytes());
        p[67] = 4; // stratum
        p[68..72].copy_from_slice(b"DFLT");
        p[77] = 1; // preferred
        p
    }

    const SIP_REGISTER: &str = concat!(
        "REGISTER sip:sip.mcast.net SIP/2.0\r\n",
        "Via: SIP/2.0/UDP 192.168.1.60:5060\r\n",
        "From: <sip:intercom@192.168.1.60>;tag=1234\r\n",
        "To: <sip:intercom@192.168.1.60>\r\n",
        "Call-ID: abc123@192.168.1.60\r\n",
        "CSeq: 1 REGISTER\r\n",
        "Contact: <sip:intercom@192.168.1.60:5060>\r\n",
        "User-Agent: WBS-Intercom/1.0\r\n",
        "Content-Length: 0\r\n\r\n",
    );

    /// Fresh state pinned to sockets mode (Python setUp).
    fn reg() -> Registry {
        let mut r = Registry::new();
        r.set_ptp_sockets_mode(true);
        r
    }

    fn snap(r: &mut Registry) -> Value {
        r.snapshot_at(now_f64(), &HashSet::new())
    }

    #[test]
    fn test_sap_announcement() {
        let pkt = parse_sap(&sap(SDP)).unwrap();
        assert_eq!(pkt["version"], 1);
        assert_eq!(pkt["deletion"], false);
        assert_eq!(pkt["msg_id_hash"], 0xBEEF);
        assert_eq!(pkt["origin_ip"], "192.168.1.50");
        assert_eq!(pkt["payload_type"], "application/sdp");
        let s = &pkt["sdp"];
        assert_eq!(s["session_name"], "Stage Box 1-2");
        assert_eq!(s["multicast_ip"], "239.69.1.10");
        assert_eq!(s["ttl"], 32);
        assert_eq!(s["port"], 5004);
        assert_eq!((&s["encoding"], &s["sample_rate"], &s["channels"]), (&json!("L24"), &json!(48000), &json!(2)));
        assert_eq!(s["ptime"], 1.0);
        assert!(s["ptime"].is_f64());
        assert_eq!(s["ptp_grandmaster"], GM_STR);
        assert_eq!(s["ptp_domain"], 0);
        assert_eq!(s["mediaclk"], "direct=0");
        // without the optional payload-type string
        let p2 = parse_sap(&sap_packet(SDP, false, 0xBEEF, [192, 168, 1, 50], false)).unwrap();
        assert_eq!(p2["sdp"]["port"], 5004);
    }

    #[test]
    fn test_sap_key_order() {
        let pkt = parse_sap(&sap(SDP)).unwrap();
        let keys: Vec<&String> = pkt["sdp"].as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec![
                "session_name", "origin", "origin_address", "multicast_ip", "ttl", "port", "encoding",
                "sample_rate", "channels", "ptime", "ptp_version", "ptp_grandmaster", "ptp_domain", "mediaclk"
            ]
        );
    }

    #[test]
    fn test_sap_deletion_drops_session() {
        let mut r = reg();
        r.handle_sap(&sap(SDP), "192.168.1.50", now_f64());
        let s = snap(&mut r);
        let sessions = s["sap"]["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        let sess = sessions[0].as_object().unwrap();
        for k in [
            "id", "origin_ip", "session_name", "multicast_ip", "port", "encoding", "sample_rate", "channels",
            "ptime", "ptp_grandmaster", "ptp_domain", "last_seen", "age_s",
        ] {
            assert!(sess.contains_key(k), "missing {k}");
        }
        assert_eq!(sess["id"], "192.168.1.50#beef");
        assert_eq!(sess["announce_count"], 1);
        let del_sdp = "v=0\r\no=- 1311738121 1311738121 IN IP4 192.168.1.50\r\n";
        let dele = parse_sap(&sap_packet(del_sdp, true, 0xBEEF, [192, 168, 1, 50], true)).unwrap();
        assert_eq!(dele["deletion"], true);
        r.handle_sap(&sap_packet(del_sdp, true, 0xBEEF, [192, 168, 1, 50], true), "192.168.1.50", now_f64());
        assert_eq!(snap(&mut r)["sap"]["sessions"], json!([]));
    }

    #[test]
    fn test_sap_deletion_other_hash_matches_origin() {
        let mut r = reg();
        r.handle_sap(&sap(SDP), "192.168.1.50", now_f64());
        let del_sdp = "v=0\r\no=- 1311738121 1311738121 IN IP4 192.168.1.50\r\n";
        r.handle_sap(&sap_packet(del_sdp, true, 0x1234, [192, 168, 1, 50], true), "192.168.1.50", now_f64());
        assert_eq!(snap(&mut r)["sap"]["sessions"], json!([]));
    }

    #[test]
    fn test_sap_reannounce_counts() {
        let mut r = reg();
        r.handle_sap(&sap(SDP), "192.168.1.50", now_f64());
        r.handle_sap(&sap(SDP), "192.168.1.50", now_f64());
        assert_eq!(snap(&mut r)["sap"]["sessions"][0]["announce_count"], 2);
    }

    #[test]
    fn test_sap_expiry() {
        let mut r = reg();
        r.handle_sap(&sap(SDP), "192.168.1.50", 1.0); // ancient
        assert_eq!(snap(&mut r)["sap"]["sessions"], json!([]));
    }

    #[test]
    fn test_sap_compressed() {
        // zlib.compress(SDP.encode()) with the compressed bit set
        let z = inflate_test_vector();
        let mut p = vec![(1u8 << 5) | 0x01, 0];
        p.extend_from_slice(&0xBEEFu16.to_be_bytes());
        p.extend_from_slice(&[192, 168, 1, 50]);
        p.extend(z);
        let pkt = parse_sap(&p).unwrap();
        assert_eq!(pkt["compressed"], true);
        assert_eq!(pkt["sdp"]["session_name"], "Stage Box 1-2");
    }

    fn inflate_test_vector() -> Vec<u8> {
        // Python: zlib.compress(SDP.encode(), 9).hex()
        let hex = "78da4d8ec14ac4301445f785fec3fb81d7be97a4310964e1ccb45010292aee431ba56869e864c4cfb71517ee2e9c73e07e792a8bd523b064be938605ff9ffd23f48302b6a2626d2aae9a5dbffae71cde239cd66f60146531fa3f4f485b69bb6b4cb5dc41f6047b107cbee216dfc6cf0f9772f27ddbb6dc188382c83822e40b9e19bb0ebb1659a054d868f71b2e719ac3d14df316c77cbc5d7cb84df30a0d9182a797a1be7f1dc0eac3de725a427256c38350b53244548b03a43c2fd17159fc0072ce3ad1";
        (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn test_ptp2_announce() {
        let m = parse_ptp(&ptp2_announce(GM, GM, 3)).unwrap();
        assert_eq!(m["version"], 2);
        assert_eq!(m["message_name"], "Announce");
        assert_eq!(m["domain"], 3);
        assert_eq!(m["clock_identity"], GM_STR);
        assert_eq!(m["sequence_id"], 42);
        let a = &m["announce"];
        assert_eq!(a["grandmaster_identity"], GM_STR);
        assert_eq!((&a["priority1"], &a["priority2"]), (&json!(128), &json!(127)));
        assert_eq!(a["clock_class"], 248);
        assert_eq!(a["clock_accuracy"], "0x20");
        assert_eq!(a["offset_scaled_log_variance"], "0x4E5D");
        assert_eq!(a["steps_removed"], 0);
        assert_eq!(a["time_source"], "INTERNAL_OSCILLATOR");
        assert_eq!(a["current_utc_offset"], 37);

        let mut r = reg();
        r.handle_ptp(&ptp2_announce(GM, GM, 0), "192.168.1.70", now_f64());
        let s = snap(&mut r);
        let p = &s["ptp"];
        let c = &p["clocks"][0];
        assert_eq!(c["network"], "ravenna/aes67");
        assert_eq!(c["is_grandmaster"], true);
        assert_eq!(c["announce_count"], 1);
        assert_eq!(p["grandmasters"][0]["identity"], GM_STR);
        assert_eq!(p["grandmasters"][0]["source_ip"], "192.168.1.70");
        assert_eq!(p["mode"], "sockets");
        let dante: HashSet<String> = ["192.168.1.70".to_string()].into_iter().collect();
        assert_eq!(r.snapshot_at(now_f64(), &dante)["ptp"]["clocks"][0]["network"], "dante (aes67 mode)");
    }

    #[test]
    fn test_dante_provider() {
        set_dante_ip_provider(Box::new(|| vec!["192.168.1.70".into(), String::new()]));
        let ips = dante_ips();
        clear_dante_ip_provider();
        assert_eq!(ips.len(), 1);
        assert!(ips.contains("192.168.1.70"));
        assert!(dante_ips().is_empty());
    }

    #[test]
    fn test_ptp1_sync() {
        let m = parse_ptp(&ptp1_sync()).unwrap();
        assert_eq!(m["version"], 1);
        assert_eq!(m["subdomain"], "_DFLT");
        assert_eq!(m["message_name"], "Sync");
        assert_eq!(m["message_type"], 1);
        assert_eq!(m["clock_identity"], "00-1D-C1-12-34-56");
        assert_eq!(m["sequence_id"], 7);
        let s = &m["sync"];
        assert_eq!(s["grandmaster_identity"], "00-1D-C1-12-34-56");
        assert_eq!(s["grandmaster_stratum"], 4);
        assert_eq!(s["grandmaster_identifier"], "DFLT");
        assert_eq!(s["grandmaster_preferred"], true);

        let mut r = reg();
        r.handle_ptp(&ptp1_sync(), "192.168.1.80", now_f64());
        let sn = snap(&mut r);
        let p = &sn["ptp"];
        let c = &p["clocks"][0];
        assert_eq!(c["network"], "dante");
        assert_eq!(c["domain"], "_DFLT");
        assert_eq!(c["sync_count"], 1);
        assert_eq!(c["is_grandmaster"], true);
        assert_eq!(c["clock_identifier"], "DFLT");
        assert_eq!(p["grandmasters"][0]["network"], "dante");
    }

    #[test]
    fn test_ptp_clock_key_order() {
        let mut r = reg();
        r.handle_ptp(&ptp1_sync(), "192.168.1.80", now_f64());
        let sn = snap(&mut r);
        let keys: Vec<&String> = sn["ptp"]["clocks"][0].as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec![
                "clock_identity", "version", "domain", "source_ip", "network", "grandmaster_identity",
                "is_grandmaster", "priority1", "priority2", "clock_class", "clock_accuracy",
                "offset_scaled_log_variance", "steps_removed", "time_source", "sync_count", "follow_up_count",
                "delay_req_count", "announce_count", "other_count", "last_message", "last_seen",
                "clock_identifier", "grandmaster_preferred", "age_s"
            ]
        );
        let pk: Vec<&String> = sn["ptp"].as_object().unwrap().keys().collect();
        assert_eq!(
            pk,
            vec!["listening", "error", "groups", "packets", "joined_interfaces", "join_errors", "bound_ports", "clocks", "grandmasters", "mode"]
        );
    }

    #[test]
    fn test_ptp_expiry() {
        let mut r = reg();
        r.handle_ptp(&ptp1_sync(), "192.168.1.80", now_f64() - 31.0);
        assert_eq!(snap(&mut r)["ptp"]["clocks"], json!([]));
    }

    #[test]
    fn test_ptp_rejects_garbage() {
        assert!(parse_ptp(&[0u8; 10]).is_err());
    }

    #[test]
    fn test_sip_register() {
        let m = parse_sip(SIP_REGISTER.as_bytes()).unwrap();
        assert_eq!(m["method"], "REGISTER");
        assert_eq!(m["is_request"], true);
        assert_eq!(m["from"], "<sip:intercom@192.168.1.60>;tag=1234");
        assert_eq!(m["to"], "<sip:intercom@192.168.1.60>");
        assert_eq!(m["call_id"], "abc123@192.168.1.60");
        assert_eq!(m["contact"], "<sip:intercom@192.168.1.60:5060>");
        assert_eq!(m["user_agent"], "WBS-Intercom/1.0");
        assert_eq!(m["cseq"], "1 REGISTER");
        let mut r = reg();
        r.handle_sip(SIP_REGISTER.as_bytes(), "192.168.1.60", now_f64());
        r.handle_sip(SIP_REGISTER.as_bytes(), "192.168.1.60", now_f64());
        let sn = snap(&mut r);
        let msgs = sn["sip"]["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["count"], 2);
        assert_eq!(msgs[0]["source_ip"], "192.168.1.60");
        let keys: Vec<&String> = msgs[0].as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec!["method", "from", "count", "to", "call_id", "contact", "user_agent", "start_line", "source_ip", "last_seen", "age_s"]
        );
    }

    #[test]
    fn test_sip_response_and_cap() {
        let m = parse_sip(b"SIP/2.0 200 OK\r\nf: a\r\nServer: X\r\n\r\n").unwrap();
        assert_eq!(m["method"], "200 OK");
        assert_eq!(m["is_request"], false);
        assert_eq!(m["from"], "a");
        assert_eq!(m["user_agent"], "X");
        assert!(parse_sip(b"HELLO\r\n").is_err());
        let mut r = reg();
        for i in 0..60 {
            let msg = format!("OPTIONS sip:x SIP/2.0\r\nFrom: u{i}\r\n\r\n");
            r.handle_sip(msg.as_bytes(), "10.0.0.1", 1000.0 + i as f64);
        }
        assert_eq!(r.sip_messages.len(), SIP_MAX_ENTRIES);
        assert_eq!(r.sip_messages.items[0].1["from"], "u10");
    }

    #[test]
    fn test_snapshot_contract() {
        let mut r = reg();
        let sn = snap(&mut r);
        let keys: Vec<&String> = sn.as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["iface", "sap", "ptp", "sip"]);
        for k in ["sap", "ptp", "sip"] {
            for f in ["listening", "error", "groups", "packets"] {
                assert!(sn[k].get(f).is_some(), "{k}.{f}");
            }
        }
        assert_eq!(sn["sap"]["groups"], json!(["239.255.255.255:9875", "224.2.127.254:9875"]));
        assert_eq!(sn["ptp"]["groups"], json!(["224.0.1.129:319", "224.0.1.129:320"]));
        assert_eq!(sn["sip"]["groups"], json!(["224.0.1.75:5060"]));
    }

    #[test]
    fn the_ptp_leader_is_an_aes67_v2_grandmaster() {
        let gms = json!([
            {"version": 1, "domain": "_DFLT", "network": "dante", "identity": "00-1d-c1-ff-fe-00-00-01", "priority1": 0},
            {"version": 2, "domain": 0, "network": "avb (gPTP)", "identity": "00-1d-c1-ff-fe-00-00-02", "priority1": 0},
            {"version": 2, "domain": 1, "network": "dante", "identity": "00-1d-c1-ff-fe-00-00-03", "priority1": 10},
            {"version": 2, "domain": 0, "network": "aes67", "identity": "00-0a-92-ff-fe-02-77-5b", "priority1": 128}
        ]);
        assert_eq!(pick_ptp_leader(&gms, None), Some(("00-0a-92-ff-fe-02-77-5b".into(), 0)));
        assert_eq!(pick_ptp_leader(&json!([]), None), None);
        // Two segments on domain 44: the leader is the one on the NIC the streams leave from.
        let split = json!([
            {"version": 2, "domain": 44, "network": "ravenna/aes67", "identity": "00-07-f5-ff-fe-42-06-a2", "priority1": 20, "interface": "enp12s0"},
            {"version": 2, "domain": 44, "network": "ravenna/aes67", "identity": "00-07-f5-ff-fe-00-54-72", "priority1": 220, "interface": "enp5s0f0"}
        ]);
        assert_eq!(pick_ptp_leader(&split, Some("enp5s0f0")).unwrap().0, "00-07-f5-ff-fe-00-54-72");
        assert_eq!(pick_ptp_leader(&split, Some("enp12s0")).unwrap().0, "00-07-f5-ff-fe-42-06-a2");
        assert_eq!(
            crate::aes67::refclk(pick_ptp_leader(&gms, None)),
            "ptp=IEEE1588-2008:00-0A-92-FF-FE-02-77-5B:0"
        );
    }

    #[test]
    fn ptp_events_report_what_changed() {
        let gm = |id: &str, lead: bool, health: &str, p1: i64| {
            json!({"clock_identity": id, "interface": "enp12s0", "version": 2, "domain": 44, "network": "aes67",
                   "is_grandmaster": lead, "sync_health": health, "priority1": p1, "clock_class": 6})
        };
        let a = vec![gm("A", true, "stable", 20)];
        let ev = ptp_events(&[], &a);
        assert_eq!(ev.iter().map(|e| e["kind"].as_str().unwrap()).collect::<Vec<_>>(), ["heard", "leader"]);
        assert_eq!(ev[1]["text"], "A leads PTPv2 domain 44 on enp12s0");
        let b = vec![gm("A", true, "contested", 10), gm("B", true, "contested", 20)];
        let kinds: Vec<String> = ptp_events(&a, &b).iter().map(|e| e["kind"].as_str().unwrap().to_string()).collect();
        assert_eq!(kinds, ["contested", "changed", "heard", "leader"]);
        assert_eq!(ptp_events(&b, &[])[0]["kind"], "gone");
        assert!(ptp_events(&a, &a).is_empty());
    }

    #[test]
    fn ptp_offset_is_ptp_minus_host() {
        let o = ptp_offset(&json!({"apparent_offset_ns": 1500, "offset_spread_ns": 20.0, "offset_samples": 9}));
        assert_eq!(o["ptp_minus_ntp_ns"], -1500.0);
        assert_eq!(ptp_offset(&json!({"apparent_offset_ns": null})), Value::Null);
    }

    #[test]
    fn apk_rows_feed_the_discovery_parser() {
        let snap = json!({"clocks": [
            {"clock_id": "00:1d:c1:ff:fe:0e:10:64", "role": "grandmaster", "variant": "PTPv2", "domain": 0,
             "source": "44.44.44.12", "grandmaster": "00:1d:c1:ff:fe:0e:10:64", "gm_class": 6, "gm_accuracy": "within 100 ns",
             "time_source": "GPS", "steps_removed": 0, "priority1": 100, "priority2": 128, "subdomain": null,
             "messages": "Sync ×8, Announce ×2", "diagnostics": {"verdict": "stable"}},
            {"clock_id": "00:0a:92:ff:fe:01:56:a3", "role": "clock", "variant": "gPTP", "domain": 0,
             "source": "00:0A:92:01:56:A3", "grandmaster": null, "subdomain": null, "messages": "Pdelay_Req ×3",
             "diagnostics": {"verdict": "unknown"}}
        ]});
        let rows = parse_ptp_discovery(&apk_rows(&snap, &|_| Some("Audinate".into()))).unwrap();
        assert_eq!(rows[0]["clock_identity"], "00-1d-c1-ff-fe-0e-10-64");
        assert_eq!(rows[0]["is_grandmaster"], true);
        assert_eq!(rows[0]["source_ip"], "44.44.44.12");
        assert_eq!(rows[0]["sync_count"], 8);
        assert_eq!(rows[0]["online"], true);
        assert_eq!(rows[1]["source_ip"], Value::Null); // gPTP: a MAC, not an IP
        assert_eq!(rows[1]["network"], "avb (gPTP)");
        assert_eq!(rows[1]["grandmaster_identity"], Value::Null);
    }

    #[test]
    fn apk_fix_asks_for_capabilities_then_the_firewall() {
        let denied = vec!["UDP port 319 unavailable (permission denied — PTP needs privileged ports)".to_string()];
        let f = apk_fix(&denied, false, "/x/tap").unwrap();
        assert!(f["options"][0]["command"].as_str().unwrap().contains("setcap cap_net_raw,cap_net_bind_service+ep /x/tap"));
        assert!(apk_fix(&[], true, "/x/tap").unwrap()["title"].as_str().unwrap().contains("firewall"));
        assert!(apk_fix(&[], false, "/x/tap").is_none());
    }

    #[test]
    fn test_mqtt_snapshot_contract() {
        let mut r = Registry::new();
        r.set_ptp_sockets_mode(false);
        r.mqtt_rows = parse_ptp_discovery(&discovery_rows()).unwrap();
        r.mqtt.updated = Some(100.0);
        r.mqtt.connected = true;
        r.mqtt.messages = 1;
        let p = r.snapshot_at(102.5, &HashSet::new())["ptp"].clone();
        let keys: Vec<&String> = p.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec![
                "listening", "error", "mode", "source", "groups", "fix", "events", "packets", "joined_interfaces", "join_errors",
                "bound_ports", "updated_age_s", "clocks", "grandmasters"
            ]
        );
        assert_eq!(p["updated_age_s"], 2.5);
        assert_eq!(p["mode"], "passive");
        assert_eq!(p["clocks"][0]["age_s"], Value::Null);
        assert_eq!(p["clocks"][0]["last_seen"], 100.0);
        // v1 (Dante) clock sending Sync is listed as a master and sorts first
        assert_eq!(p["grandmasters"][0]["version"], 1);
        assert_eq!(p["grandmasters"][1]["identity"], "00-07-F5-FF-FE-42-06-A2");
    }

    fn discovery_rows() -> Value {
        json!([
            {"clock_id": "00:07:F5:FF:FE:42:06:A2", "variant": "PTPv2/UDP", "vendor": "Bridgeco Co AG",
             "domain": "44", "subdomain": "-", "role": "grandmaster", "grandmaster": "00:07:F5:FF:FE:42:06:A2",
             "priority1": "200", "priority2": "200", "gm_class": "255", "gm_accuracy": "100ns",
             "steps_removed": "0", "sync_health": "contested", "_row_state": "online",
             "messages": "Sync \u{00d7}11829, Follow_Up \u{00d7}10062, Announce \u{00d7}4981"},
            {"clock_id": "00:0A:92:FF:FE:02:77:5B", "variant": "PTPv1", "vendor": "Presonus Corporation",
             "domain": "0", "subdomain": "_DFLT", "role": "clock", "grandmaster": "-", "priority1": "-",
             "_row_state": "online", "messages": "Sync \u{00d7}45950, Follow_Up \u{00d7}39562"}
        ])
    }

    #[test]
    fn test_ptp_discovery_mapping() {
        let out = parse_ptp_discovery(&discovery_rows()).unwrap();
        let (a, b) = (&out[0], &out[1]);
        assert_eq!(a["clock_identity"], "00-07-F5-FF-FE-42-06-A2");
        assert_eq!((&a["version"], &a["domain"], &a["network"]), (&json!(2), &json!(44), &json!("ravenna/aes67")));
        assert_eq!(a["is_grandmaster"], true);
        assert_eq!((&a["priority1"], &a["sync_count"], &a["announce_count"]), (&json!(200), &json!(11829), &json!(4981)));
        assert_eq!((&b["version"], &b["domain"], &b["network"]), (&json!(1), &json!("_DFLT"), &json!("dante")));
        assert_eq!(b["grandmaster_identity"], Value::Null);
        assert_eq!(b["sync_count"], 45950);
        assert_eq!(b["priority1"], Value::Null);
        let keys: Vec<&String> = a.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec![
                "clock_identity", "version", "variant", "domain", "source_ip", "network", "vendor",
                "grandmaster_identity", "is_grandmaster", "priority1", "priority2", "clock_class", "clock_accuracy",
                "steps_removed", "time_source", "sync_health", "online", "sync_count", "follow_up_count",
                "delay_req_count", "announce_count", "last_seen_text", "ptp_offset", "interface"
            ]
        );
    }

    #[test]
    fn test_ptp_discovery_garbage() {
        assert_eq!(parse_ptp_discovery(&json!({"not": "a list"})).unwrap(), Vec::<Value>::new());
        assert!(parse_ptp_discovery(&json!([1])).is_err());
    }

    #[test]
    fn test_mqtt_packet_framing() {
        assert_eq!(mqtt_packet(0xC0, b""), vec![0xC0, 0x00]);
        let big = vec![0u8; 321];
        let p = mqtt_packet(0x30, &big);
        assert_eq!(&p[..3], &[0x30, 0xC1, 0x02]);
        assert_eq!(mqtt_str("MQTT"), b"\x00\x04MQTT".to_vec());
    }

    #[test]
    fn test_describe_error() {
        let e = OpenErr::from(io::Error::from_raw_os_error(libc::EACCES));
        assert!(describe_error(&e, 319).starts_with("bind UDP 319: [Errno 13] Permission denied; needs CAP_NET_BIND_SERVICE"));
        let e = OpenErr::from(io::Error::from_raw_os_error(libc::EADDRINUSE));
        assert_eq!(describe_error(&e, 9875), "UDP 9875: OSError: [Errno 98] Address already in use");
    }

    /// Binds the real SAP / SIP ports and joins the groups (network; run with --ignored).
    #[test]
    #[ignore]
    fn live_open_socket_joins() {
        let (_s, joined, _errs) = open_socket(&["239.255.255.255", "224.2.127.254"], 9875).ok().expect("bind 9875");
        assert!(!joined.is_empty());
    }

    /// Listens for SAP for 3 s and prints the snapshot (network; run with --ignored --nocapture).
    #[test]
    #[ignore]
    fn live_sap_listen() {
        let stop = Arc::new(AtomicBool::new(false));
        start(stop.clone());
        std::thread::sleep(Duration::from_secs(3));
        let s = snapshot();
        println!("{}", serde_json::to_string_pretty(&s).unwrap());
        stop.store(true, Ordering::Relaxed);
        assert_eq!(s["sap"]["listening"], true);
    }
}
