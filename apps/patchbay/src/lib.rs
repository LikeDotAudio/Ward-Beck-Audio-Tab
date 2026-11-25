// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! Port of ward-beck-audio-tab.py (daemon core): virtual sinks/routes, source list and
//! per-source meters, test tone, Bit Buddy, Reset All / restart / abandon.
//! The other applications reach it only through its service (wbs_plugin::services::patchbay()),
//! and it reaches them only through theirs.
//! (Python module globals AVAILABLE_SOURCES, SOURCE_METERS, PHYSICAL_SINK, SELECTED_SOURCE.)
//!
//! Locking: `state_lock()` is the Python STATE_LOCK (a re-entrant lock that serialises whole
//! operations). The data itself lives behind a private std Mutex that is only ever held for
//! a few field reads/writes, never across a command, another module, or a sleep, so the
//! pub accessors are safe to call with or without `state_lock()` held.
//!
//! Dry run: WBS_DRY_RUN=1 makes every side effect on the live audio graph a logged no-op
//! (pactl load/unload-module, set-default-sink, move-sink-input, killing stray processes,
//! pacat generator playback, the CBC pipeline stop in Reset All). Read-only commands
//! (pactl list, parec meters, ps) still run. Only for testing the HTTP layer side by side
//! with a live daemon (WBS_PORT=18467 WBS_SHM_BASE=<scratch dir> WBS_DRY_RUN=1).

// Plugged into the host: routes, services and start-up (plugin.rs).
pub mod plugin;
pub use plugin::plug;

use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use wbs_common::pactl::{run_cmd, run_mut};
use wbs_common::{dry_run, guard, state_lock};
use wbs_plugin::services;
use wbs_common::{is_shutting_down, now_f64};

pub use wbs_pyshim as py;


/// Run `f`, swallowing a panic (Python: try: ... except Exception: pass).
// ---------------------------------------------------------------------------
// Daemon state (Python module globals)
// ---------------------------------------------------------------------------
pub const ROUTE_NAMES: [&str; 4] = ["meter_tap", "insert_stage", "speaker_split", "test_point_split"];

struct ProcSlot {
    pid: u32,
    alive: bool, // cleared (under the data lock) before the owner reaps it: no pid-reuse kills
}

struct Data {
    managed_modules: Vec<i64>,
    meter_levels: [f64; 4], // left_db, right_db, left_vu, right_vu
    routes: [bool; 4],      // ROUTE_NAMES order
    physical_sink: String,
    selected_source: String,
    available_sources: Vec<Value>,
    source_meters: Map<String, Value>,
    source_threads: HashMap<String, JoinHandle<()>>,
    source_procs: Vec<(String, ProcSlot)>, // dict insertion order
    retired: HashSet<String>,
    tone_active: bool,
    tone_freq: i64,
    tone: ToneSpec,
    tone_pid: Option<u32>,
    bb_active: bool,
    bb_config: Map<String, Value>,
    bb_pid: Option<u32>,
}

use wbs_generators::spitter::{SpitterFile, SpitterParams, SpitterSynth};

fn default_bb_config() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("pattern".into(), json!("sweep"));
    m.insert("pro_mode".into(), json!(true));
    m.insert("audio_data".into(), json!(true));
    m.insert("sample_rate".into(), json!(48000));
    m
}

fn d() -> MutexGuard<'static, Data> {
    static DATA: OnceLock<Mutex<Data>> = OnceLock::new();
    DATA.get_or_init(|| {
        Mutex::new(Data {
            managed_modules: Vec::new(),
            meter_levels: [-60.0, -60.0, -42.0, -42.0],
            routes: [true; 4],
            physical_sink: String::new(),
            selected_source: "wbs_input_bus.monitor".into(),
            available_sources: Vec::new(),
            source_meters: Map::new(),
            source_threads: HashMap::new(),
            source_procs: Vec::new(),
            retired: HashSet::new(),
            tone_active: false,
            tone_freq: 1000,
            tone: ToneSpec::default(),
            tone_pid: None,
            bb_active: false,
            bb_config: default_bb_config(),
            bb_pid: None,
        })
    })
    .lock()
    .unwrap_or_else(|e| e.into_inner())
}

fn meter(db: f64, vu: f64) -> Value {
    json!({"db": db, "vu": vu})
}

/// AVAILABLE_SOURCES: list of source dicts (each has at least "name").
pub fn available_sources() -> Vec<Value> {
    d().available_sources.clone()
}
/// SOURCE_METERS: {source_name: {"db": f, "vu": f}}.
pub fn source_meters() -> Value {
    Value::Object(d().source_meters.clone())
}
pub fn physical_sink() -> String {
    d().physical_sink.clone()
}
pub fn selected_source() -> String {
    d().selected_source.clone()
}
pub fn set_selected_source(name: &str) {
    d().selected_source = name.to_string();
}

/// METER_LEVELS.
pub fn meter_levels_json() -> Value {
    let m = d().meter_levels;
    json!({"left_db": m[0], "right_db": m[1], "left_vu": m[2], "right_vu": m[3]})
}

/// ACTIVE_ROUTES.
pub fn routes_json() -> Value {
    let r = d().routes;
    let mut m = Map::new();
    for (i, n) in ROUTE_NAMES.iter().enumerate() {
        m.insert((*n).into(), Value::Bool(r[i]));
    }
    Value::Object(m)
}

/// {"active": TEST_TONE_ACTIVE, "freq": TEST_TONE_FREQ}
pub fn test_tone_json() -> Value {
    let g = d();
    json!({"active": g.tone_active, "freq": g.tone_freq, "freq_r": g.tone.freq_r, "level_db": g.tone.level_db, "channel": g.tone.channel})
}

pub fn test_tone_active() -> bool {
    d().tone_active
}

/// {"active": BITBUDDY_ACTIVE, "config": BITBUDDY_CONFIG}
pub fn bitbuddy_json() -> Value {
    let g = d();
    json!({"active": g.bb_active, "config": Value::Object(g.bb_config.clone())})
}

pub fn bitbuddy_active() -> bool {
    d().bb_active
}

pub fn bitbuddy_config() -> Map<String, Value> {
    d().bb_config.clone()
}

/// POST /api/toggle: flips a known route and rebuilds (caller holds state_lock).
pub fn toggle_route(route: &Value) {
    let Some(name) = route.as_str() else { return };
    let Some(i) = ROUTE_NAMES.iter().position(|n| *n == name) else { return };
    {
        let mut g = d();
        g.routes[i] = !g.routes[i];
    }
    apply_routes();
}

/// POST /api/force_speakers.
pub fn force_speakers() {
    d().routes[2] = true;
    apply_routes();
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Python run_cmd: (returncode, stdout.strip(), stderr.strip()) with a 2.5 s timeout;
/// (-1, "", error) when the command cannot run or times out.
/// Signal one of the daemon's own children (meter parecs, tone / Bit Buddy pacat).
/// Always real, also in dry run: the daemon must clean up what it started.
fn kill_pid(pid: u32, sig: i32) -> bool {
    wbs_sys::kill(pid, sig)
}

/// Python _kill_pid: SIGTERM, True if delivered. Used by Reset All on *other*
/// programs' stray processes, so dry run only logs it.
fn _kill_pid(pid: u32) -> bool {
    if dry_run() {
        println!("[DRY RUN] kill -15 {pid}");
        return false;
    }
    kill_pid(pid, wbs_sys::SIGTERM)
}

/// `pactl list short ...` style tab-split lines.
fn tab_lines(out: &str) -> Vec<Vec<&str>> {
    out.lines().map(|l| l.split('\t').collect()).collect()
}

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------

/// Parse `pactl list sources` into [{name, description}].
pub fn parse_pactl_sources(out: &str) -> Vec<Value> {
    let mut sources = Vec::new();
    let mut cur_name: Option<String> = None;
    let mut cur_desc: Option<String> = None;
    let push = |sources: &mut Vec<Value>, n: &Option<String>, d: &Option<String>| {
        if let Some(n) = n {
            if !n.is_empty() {
                let desc = d.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| n.clone());
                sources.push(json!({"name": n, "description": desc}));
            }
        }
    };
    for line in out.lines() {
        let s = line.trim();
        if let Some(rest) = s.strip_prefix("Name: ") {
            push(&mut sources, &cur_name, &cur_desc);
            cur_name = Some(rest.trim().to_string());
            cur_desc = None;
        } else if let Some(rest) = s.strip_prefix("Description: ") {
            cur_desc = Some(rest.trim().to_string());
        }
    }
    push(&mut sources, &cur_name, &cur_desc);
    sources
}

fn sget<'a>(v: &'a Value, k: &str) -> Option<&'a Value> {
    v.as_object().and_then(|o| o.get(k))
}

fn arr(v: Option<&Value>) -> &[Value] {
    v.and_then(|x| x.as_array()).map(|a| a.as_slice()).unwrap_or(&[])
}

/// NMOS IS-04 flows of every discovered node as sources.
pub fn nmos_sources(nodes: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for n in nodes {
        let node_name = py::or(py::or(sget(n, "hostname"), sget(n, "label")), sget(n, "ip")).cloned().unwrap_or(Value::Null);
        let ip = py::str_or(sget(n, "ip"), "None");
        for flw in arr(sget(n, "flows")) {
            let id = sget(flw, "id").cloned().unwrap_or(Value::Null);
            let flw_lbl = match sget(flw, "label") {
                Some(l) if py::truthy(l) => l.clone(),
                _ => Value::String(py::head(sget(flw, "id").and_then(|v| v.as_str()).unwrap_or(""), 8)),
            };
            let tags = sget(flw, "tags");
            let tag = |k: &str, d: &str| -> Value {
                match tags.and_then(|t| sget(t, k)) {
                    Some(Value::Array(a)) => a.first().cloned().unwrap_or(Value::Null),
                    Some(other) => other.clone(),
                    None => Value::String(d.into()),
                }
            };
            let chans = tag("channels", "8");
            let rate = tag("clockRate", "48000");
            let fmt = tag("encodingName", "L24");
            let mut m = Map::new();
            m.insert("name".into(), json!(format!("nmos:{}:{}", ip, py::str_of(&id))));
            m.insert(
                "description".into(),
                json!(format!(
                    "\u{1F4E1} NMOS: {} \u{2014} {} ({}ch {} @ {}Hz)",
                    py::str_of(&node_name),
                    py::str_of(&flw_lbl),
                    py::str_of(&chans),
                    py::str_of(&fmt),
                    py::str_of(&rate)
                )),
            );
            m.insert("nmos".into(), json!(true));
            m.insert("node_ip".into(), sget(n, "ip").cloned().unwrap_or(Value::Null));
            m.insert("node_label".into(), node_name.clone());
            m.insert("flow_id".into(), id);
            m.insert("flow_label".into(), flw_lbl);
            m.insert("channels".into(), chans);
            m.insert("rate".into(), rate);
            out.push(Value::Object(m));
        }
    }
    out
}

/// MXL audio flows in every other domain (engine.scan_all_domains()) as sources.
pub fn mxl_sources(all_domains: &Value, own_domain: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let Some(doms) = all_domains.as_object() else { return out };
    for (dom_lbl, dom) in doms {
        if dom_lbl == own_domain {
            continue;
        }
        let low = dom_lbl.to_lowercase();
        let is_wbs = low.contains("ward-beck") || low.contains("wbs");
        for flw in arr(sget(dom, "flows")) {
            let s = |k: &str| match sget(flw, k) {
                Some(Value::String(x)) => x.to_lowercase(),
                _ => String::new(),
            };
            let has_cc = flw.as_object().map(|o| o.contains_key("channel_count")).unwrap_or(false);
            if !(s("format").contains("audio") || s("media_type").contains("audio") || has_cc) {
                continue;
            }
            let flw_id = py::or(sget(flw, "flow_id"), sget(flw, "id")).cloned().unwrap_or(Value::Null);
            let flw_lbl = match sget(flw, "label") {
                Some(l) if py::truthy(l) => l.clone(),
                _ => Value::String(py::head(&py::str_of(&flw_id), 8)),
            };
            let prefix = if is_wbs {
                "\u{26A1} WBS TEST SIGNAL".to_string()
            } else {
                format!("\u{1F39B}\u{FE0F} CBC MXL ({dom_lbl})")
            };
            let mut m = Map::new();
            m.insert("name".into(), json!(format!("mxl:{}:{}", dom_lbl, py::str_of(&flw_id))));
            m.insert(
                "description".into(),
                json!(format!(
                    "{}: {} \u{2014} {}",
                    prefix,
                    py::str_of(&flw_lbl),
                    py::str_or(sget(flw, "description"), "")
                )),
            );
            m.insert("mxl".into(), json!(true));
            m.insert("domain".into(), json!(dom_lbl));
            m.insert("flow_id".into(), flw_id);
            m.insert("flow_label".into(), flw_lbl);
            m.insert("is_wbs_test_signal".into(), json!(is_wbs));
            out.push(Value::Object(m));
        }
    }
    out
}

/// Dante / Ravenna / AES67 SAP streams from the AoIP telemetry as sources.
pub fn aoip_sources(aoip: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for dev in arr(sget(aoip, "dante").and_then(|x| sget(x, "devices"))) {
        let dname = py::str_or(sget(dev, "name"), "Metro16-DANTE2AVB");
        let engine = py::str_or(sget(dev, "engine"), "DCM");
        let ip = sget(dev, "ip").cloned().unwrap_or(Value::Null);
        for (pair, label) in [("Ch1-Ch2", "Ch 1 & 2"), ("Ch3-Ch4", "Ch 3 & 4")] {
            let mut m = Map::new();
            m.insert("name".into(), json!(format!("dante:{dname}:{pair}")));
            m.insert(
                "description".into(),
                json!(format!("\u{1F3B5} DANTE: {dname} \u{2014} {label} ({engine} @ 48kHz L24)")),
            );
            m.insert("dante".into(), json!(true));
            m.insert("device".into(), json!(dname));
            m.insert("ip".into(), ip.clone());
            out.push(Value::Object(m));
        }
    }
    for rst in arr(sget(aoip, "ravenna").and_then(|x| sget(x, "streams"))) {
        let rname = py::str_or(sget(rst, "name"), "Stream");
        let ip = py::str_or(sget(rst, "ip"), "None");
        let port = py::str_or(sget(rst, "port"), "None");
        let mut m = Map::new();
        m.insert("name".into(), json!(format!("ravenna:{ip}:{rname}")));
        m.insert(
            "description".into(),
            json!(format!("\u{1F4FB} RAVENNA: {rname} \u{2014} rtsp://{ip}:{port}/{rname} (Archwave uNet2500)")),
        );
        m.insert("ravenna".into(), json!(true));
        m.insert("stream_name".into(), json!(rname));
        m.insert("url".into(), sget(rst, "url").cloned().unwrap_or(Value::Null));
        out.push(Value::Object(m));
    }
    for sap in arr(sget(aoip, "aes67_sap").and_then(|x| sget(x, "active_announcements"))) {
        let sname = py::str_or(sget(sap, "session_name"), "Ward-Beck-AES67-Master");
        let mip = py::str_or(sget(sap, "multicast_ip"), "None");
        let port = py::str_or(sget(sap, "port"), "None");
        let mut m = Map::new();
        m.insert("name".into(), json!(format!("aes67:{sname}")));
        m.insert(
            "description".into(),
            json!(format!(
                "\u{1F310} AES67 / SAP: {sname} \u{2014} Multicast {mip}:{port} (PTP IEEE 1588)"
            )),
        );
        m.insert("aes67".into(), json!(true));
        m.insert("session".into(), json!(sname));
        out.push(Value::Object(m));
    }
    out
}

/// Python list_sources(): pactl sources + NMOS flows + MXL flows + AoIP streams.
pub fn list_sources() -> Vec<Value> {
    let (code, out, _) = run_cmd(&["pactl", "list", "sources"]);
    let mut sources = if code == 0 { parse_pactl_sources(&out) } else { Vec::new() };

    let nodes = {
        let _g = state_lock();
        services::nmos().nodes()
    };
    sources.extend(nmos_sources(&nodes));

    if let Some(extra) = guard(|| {
        let mxl = services::mxl();
        mxl_sources(&mxl.scan_all_domains(), &mxl.domain_id())
    }) {
        sources.extend(extra);
    }

    if let Some(extra) = guard(|| aoip_sources(&services::nmos().aoip_or_inspect())) {
        sources.extend(extra);
    }
    sources
}

fn set_source_meter(name: &str, db: f64, vu: f64) {
    let _g = state_lock();
    d().source_meters.insert(name.to_string(), meter(db, vu));
}

fn is_retired(name: &str) -> bool {
    d().retired.contains(name)
}

/// 512-frame s16le chunk RMS (Python: sqrt(sum((s/32768)^2)/frames)) for one interleaved channel.
pub fn chunk_rms(data: &[u8], channels: usize, ch: usize) -> f64 {
    let frames = data.len() / (2 * channels);
    let mut sum = 0.0f64;
    for f in 0..frames {
        let o = (f * channels + ch) * 2;
        let s = i16::from_le_bytes([data[o], data[o + 1]]) as f64 / 32768.0;
        sum += s * s;
    }
    (sum / frames as f64).sqrt()
}

/// Background reader computing live RMS, dBFS and VU for one source.
pub fn source_meter_worker(src_name: String) {
    if src_name.starts_with("mxl:") {
        let parts: Vec<&str> = src_name.split(':').collect();
        let flow_id = if parts.len() >= 3 { parts[2].to_string() } else { String::new() };
        let mut vu_val = 0.0f64;
        while !is_shutting_down() {
            let _ = guard(|| {
                let wf = services::mxl().external_waveform(&flow_id);
                let maxs: Vec<f64> = arr(sget(&wf, "ch0_max")).iter().filter_map(|x| x.as_f64()).collect();
                if !maxs.is_empty() {
                    let rms = (maxs.iter().map(|x| x * x).sum::<f64>() / maxs.len() as f64).sqrt();
                    vu_val = 0.70 * vu_val + 0.30 * rms;
                    let db = 20.0 * vu_val.max(1e-5).log10();
                    set_source_meter(&src_name, py::round(db, 1), py::round(db + 18.0, 1));
                }
            });
            thread::sleep(Duration::from_millis(80));
        }
        return;
    }

    if ["nmos:", "dante:", "ravenna:", "aes67:"].iter().any(|p| src_name.starts_with(p)) {
        while !is_shutting_down() {
            set_source_meter(&src_name, -20.0, -2.0);
            thread::sleep(Duration::from_secs(1));
        }
        return;
    }

    const CHUNK_FRAMES: usize = 512;
    let mut buf = vec![0u8; CHUNK_FRAMES * 2]; // s16le mono
    let mut vu_val = 0.0f64;
    while !is_shutting_down() && !is_retired(&src_name) {
        let spawned = Command::new("parec")
            .args([
                &format!("--device={src_name}"),
                "--format=s16le",
                "--rate=24000",
                "--channels=1",
                "--latency-msec=35",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = spawned {
            let pid = child.id();
            {
                let _g = state_lock();
                let mut g = d();
                let slot = ProcSlot { pid, alive: true };
                if let Some(e) = g.source_procs.iter_mut().find(|(n, _)| *n == src_name) {
                    e.1 = slot;
                } else {
                    g.source_procs.push((src_name.clone(), slot));
                }
            }
            if let Some(mut out) = child.stdout.take() {
                while !is_shutting_down() {
                    if out.read_exact(&mut buf).is_err() {
                        break;
                    }
                    let rms = chunk_rms(&buf, 1, 0);
                    // Smooth VU ballistic filter (300ms integration simulation)
                    vu_val = 0.70 * vu_val + 0.30 * rms;
                    let db = 20.0 * vu_val.max(1e-5).log10();
                    set_source_meter(&src_name, py::round(db, 1), py::round(db + 18.0, 1));
                }
            }
            {
                let mut g = d();
                if let Some(e) = g.source_procs.iter_mut().find(|(n, s)| *n == src_name && s.pid == pid) {
                    if e.1.alive {
                        wbs_sys::kill(pid, wbs_sys::SIGTERM);
                    }
                    e.1.alive = false;
                } else {
                    wbs_sys::kill(pid, wbs_sys::SIGTERM);
                }
            }
            let _ = child.wait();
        }
        thread::sleep(Duration::from_millis(500));
    }
}

/// Python refresh_sources(): re-list sources, keep SELECTED_SOURCE valid, start meter readers.
pub fn refresh_sources() {
    let srcs = list_sources();
    let names: Vec<String> =
        srcs.iter().filter_map(|s| sget(s, "name").and_then(|n| n.as_str()).map(String::from)).collect();
    let mut g = d();
    g.available_sources = srcs.clone();
    if !names.iter().any(|n| *n == g.selected_source) {
        if names.iter().any(|n| n == "wbs_input_bus.monitor") {
            g.selected_source = "wbs_input_bus.monitor".into();
        } else if let Some(first) = names.first() {
            g.selected_source = first.clone();
        }
    }
    for s_name in names {
        if s_name.starts_with("nmos:") || s_name.starts_with("mxl:") {
            if !g.source_meters.contains_key(&s_name) {
                g.source_meters.insert(s_name.clone(), meter(-20.0, -2.0));
            }
            continue;
        }
        g.retired.remove(&s_name);
        if !g.source_meters.contains_key(&s_name) {
            g.source_meters.insert(s_name.clone(), meter(-60.0, -42.0));
        }
        let alive = g.source_threads.get(&s_name).map(|h| !h.is_finished()).unwrap_or(false);
        if !alive {
            let n = s_name.clone();
            let h = thread::spawn(move || source_meter_worker(n));
            g.source_threads.insert(s_name, h);
        }
    }
}

// ---------------------------------------------------------------------------
// Sinks, modules, routes
// ---------------------------------------------------------------------------

pub fn is_virtual_sink(name: &str) -> bool {
    if name.is_empty() {
        return true;
    }
    let low = name.to_lowercase();
    low.starts_with("wbs_")
        || ["loopback", "aloop", "snd_aloop", "snd-aloop", "null", "dummy", "virtual", "remap", "pw-loopback", "input.pw"]
            .iter()
            .any(|k| low.contains(k))
}

/// Pick the physical sink from `pactl list short sinks` output and the default sink.
pub fn pick_physical_sink(sinks_out: &str, default_sink: impl FnOnce() -> String) -> String {
    let sink_names: Vec<&str> = tab_lines(sinks_out)
        .into_iter()
        .filter(|p| p.len() >= 2 && !p[1].is_empty())
        .map(|p| p[1])
        .collect();
    // Priority 1: Real ALSA hardware output (e.g. alsa_output.pci-*)
    if let Some(s) = sink_names.iter().find(|s| s.starts_with("alsa_output.") && !is_virtual_sink(s)) {
        return s.to_string();
    }
    // Priority 2: Bluetooth speaker / headphones
    if let Some(s) = sink_names.iter().find(|s| s.starts_with("bluez_output.") && !is_virtual_sink(s)) {
        return s.to_string();
    }
    // Priority 3: Default sink if not virtual
    let def = default_sink();
    if !def.is_empty() && !is_virtual_sink(&def) {
        return def;
    }
    // Priority 4: Any non-virtual sink
    if let Some(s) = sink_names.iter().find(|s| !is_virtual_sink(s)) {
        return s.to_string();
    }
    sink_names.first().map(|s| s.to_string()).unwrap_or_default()
}

pub fn detect_physical_sink() -> String {
    let (_, sinks_out, _) = run_cmd(&["pactl", "list", "short", "sinks"]);
    pick_physical_sink(&sinks_out, || run_cmd(&["pactl", "get-default-sink"]).1)
}

pub fn move_all_streams_to(target_sink: &str) {
    if target_sink.is_empty() {
        return;
    }
    let (_, inputs_out, _) = run_cmd(&["pactl", "list", "short", "sink-inputs"]);
    for p in tab_lines(&inputs_out) {
        if py::isdigit(p[0]) {
            run_mut(&["pactl", "move-sink-input", p[0], target_sink]);
        }
    }
}

pub fn load_module(args: &[String]) -> Option<i64> {
    let mut cmd: Vec<&str> = vec!["pactl", "load-module"];
    cmd.extend(args.iter().map(|s| s.as_str()));
    let (code, out, err) = run_mut(&cmd);
    if code == 0 && py::isdigit(&out) {
        if let Ok(id) = out.parse::<i64>() {
            d().managed_modules.push(id);
            return Some(id);
        }
    }
    println!("[WARN] pactl load-module {} failed: {}", args.join(" "), err);
    None
}

/// Module ids from `pactl list short modules` whose argument column contains "wbs_".
pub fn wbs_module_ids(mods_out: &str) -> Vec<i64> {
    let mut ids: Vec<i64> = tab_lines(mods_out)
        .into_iter()
        .filter(|p| p.len() >= 3 && p[2].contains("wbs_") && py::isdigit(p[0]))
        .filter_map(|p| p[0].parse().ok())
        .collect();
    ids.sort_unstable_by(|a, b| b.cmp(a));
    ids
}

/// Complete teardown: restores the hardware default sink, stops generators, unloads all wbs_ modules.
pub fn abandon_ship() {
    stop_test_tone();
    let _ = guard(|| services::aes67().stop_all());
    let _ = guard(|| services::mxl().stop());

    let mut phys = detect_physical_sink();
    if phys.is_empty() {
        phys = physical_sink();
    }
    if !phys.is_empty() {
        run_mut(&["pactl", "set-default-sink", &phys]);
        move_all_streams_to(&phys);
    }

    let managed: Vec<i64> = std::mem::take(&mut d().managed_modules);
    for mod_id in managed.iter().rev() {
        run_mut(&["pactl", "unload-module", &mod_id.to_string()]);
    }

    let (_, mods_out, _) = run_cmd(&["pactl", "list", "short", "modules"]);
    for mod_id in wbs_module_ids(&mods_out) {
        run_mut(&["pactl", "unload-module", &mod_id.to_string()]);
    }

    // Terminate all source meter readers
    {
        let _g = state_lock();
        stop_bit_buddy();
        stop_test_tone();
        let mut g = d();
        for (_, slot) in g.source_procs.iter_mut() {
            if slot.alive {
                kill_pid(slot.pid, wbs_sys::SIGTERM);
            }
        }
        g.source_procs.clear();
    }

    println!(
        "[WARD BECK TAP] Teardown complete. Hardware sink restored: {}",
        if phys.is_empty() { "None" } else { &phys }
    );
}

pub const STRAY_MODULE_TYPES: [&str; 7] = [
    "module-null-sink",
    "module-loopback",
    "module-remap-sink",
    "module-remap-source",
    "module-combine-sink",
    "module-virtual-sink",
    "module-virtual-source",
];
/// The XTM banner-free bridge service is deliberate.
pub const KEEP_PW_LOOPBACK: [&str; 1] = ["XTM_Audio_Bridge"];

/// (pid, ppid, args) rows of `ps -o pid=,ppid=,args=`, in ps order.
pub fn parse_ps(ps_out: &str) -> Vec<(u32, u32, String)> {
    let mut rows: Vec<(u32, u32, String)> = Vec::new();
    for line in ps_out.lines() {
        let mut it = line.trim().splitn(2, char::is_whitespace);
        let a = it.next().unwrap_or("");
        let rest = it.next().unwrap_or("").trim_start();
        let mut it2 = rest.splitn(2, char::is_whitespace);
        let b = it2.next().unwrap_or("");
        let args = it2.next().unwrap_or("").trim_start();
        if !args.is_empty() && py::isdigit(a) && py::isdigit(b) {
            if let (Ok(pid), Ok(ppid)) = (a.parse(), b.parse()) {
                if let Some(e) = rows.iter_mut().find(|r| r.0 == pid) {
                    *e = (pid, ppid, args.to_string());
                } else {
                    rows.push((pid, ppid, args.to_string()));
                }
            }
        }
    }
    rows
}

/// Stray pw-loopback cards and orphaned stream helpers among ps rows (me = our pid).
pub fn stray_processes(rows: &[(u32, u32, String)], me: u32) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    for (pid, ppid, args) in rows {
        if *pid == me {
            continue;
        }
        let first = args.split_whitespace().next().unwrap_or("");
        let exe = first.rsplit('/').next().unwrap_or("");
        let parent_args = rows.iter().find(|r| r.0 == *ppid).map(|r| r.2.as_str()).unwrap_or("");
        let orphaned = *ppid == 1 || parent_args.contains("systemd");
        let stray = match exe {
            "pw-loopback" => !KEEP_PW_LOOPBACK.iter().any(|k| args.contains(k)),
            "parec" | "pacat" | "pw-cat" | "pw-play" | "pw-record" => *ppid != me && orphaned,
            _ => false,
        };
        if stray {
            out.push((*pid, args.clone()));
        }
    }
    out
}

/// Reset All: kills every generator and destroys every stray virtual card, then rebuilds
/// the WBS patch. Generators stay off until re-engaged. Returns {generators, modules, processes}.
pub fn reset_all() -> Value {
    let mut generators: Vec<String> = Vec::new();
    let mut modules: Vec<String> = Vec::new();
    let mut processes: Vec<String> = Vec::new();
    let me = std::process::id();
    if services::receivers().stop_point() {
        processes.push("test point lifted".into());
    }
    if services::receivers().stop_receivers() {
        processes.push("network / MoIP receivers stopped".into());
    }

    {
        let _g = state_lock();
        if test_tone_active() {
            generators.push("POD-33 test tone".into());
        }
        stop_test_tone();
        if bitbuddy_active() {
            generators.push("ABB-1 Bit Buddy".into());
        }
        stop_bit_buddy();
    }
    if let Some(tx) = guard(|| services::aes67().stop_all()) {
        generators.extend(tx);
    }
    match guard(|| services::mxl().stop_generators()) {
        Some(g) => generators.extend(g),
        None => println!("[WARN] MXL generator stop failed: panic"),
    }
    if dry_run() {
        println!("[DRY RUN] POST http://127.0.0.1:9600/pipeline/stop");
    } else if services::cbc().post_raw("http://127.0.0.1:9600/pipeline/stop", b"{}", 1.5).is_ok() {
        generators.push("CBC MXL test generator".into());
        services::cbc().poll_generator();
    }

    // Stray virtual cards: unmanaged pactl modules
    {
        let _g = state_lock();
        let (_, mods_out, _) = run_cmd(&["pactl", "list", "short", "modules"]);
        for p in tab_lines(&mods_out) {
            if p.len() < 2 || !py::isdigit(p[0]) {
                continue;
            }
            let Ok(mid) = p[0].parse::<i64>() else { continue };
            let managed = d().managed_modules.contains(&mid);
            if STRAY_MODULE_TYPES.contains(&p[1]) && !managed {
                let (code, _, _) = run_mut(&["pactl", "unload-module", &mid.to_string()]);
                if code == 0 {
                    let arg = if p.len() > 2 { p[2] } else { "" };
                    modules.push(format!("{} {}", p[1], arg).trim().to_string());
                }
            }
        }
    }

    // Stray processes: pw-loopback cards and orphaned stream helpers
    let uid = wbs_sys::uid();
    let (_, ps_out, _) = run_cmd(&["ps", "-u", &uid, "-o", "pid=,ppid=,args="]);
    for (pid, args) in stray_processes(&parse_ps(&ps_out), me) {
        if _kill_pid(pid) {
            processes.push(format!("{} {}", pid, py::head(&args, 90)));
        }
    }

    thread::sleep(Duration::from_millis(300)); // let PipeWire drop the destroyed nodes
    {
        let _g = state_lock();
        refresh_sources();
        let mut g = d();
        let live: HashSet<String> = g
            .available_sources
            .iter()
            .filter_map(|s| sget(s, "name").and_then(|n| n.as_str()).map(String::from))
            .collect();
        let gone: Vec<String> = g.source_procs.iter().map(|(n, _)| n.clone()).filter(|n| !live.contains(n)).collect();
        for name in gone {
            g.retired.insert(name.clone());
            if let Some(i) = g.source_procs.iter().position(|(n, _)| *n == name) {
                let (_, slot) = g.source_procs.remove(i);
                if slot.alive {
                    kill_pid(slot.pid, wbs_sys::SIGTERM);
                }
            }
            g.source_meters.shift_remove(&name);
            g.source_threads.remove(&name);
            processes.push(format!("meter reader for {name}"));
        }
        drop(g);
        apply_routes();
    }

    println!(
        "[WARD BECK TAP] Reset All: {} generators killed, {} stray modules + {} stray processes destroyed",
        generators.len(),
        modules.len(),
        processes.len()
    );
    json!({"generators": generators, "modules": modules, "processes": processes})
}

/// The four WBS null sinks: (sink_name, device.description, node.nick). The description is what
/// the desktop's sound settings list, so it says what the point carries. ASCII only: pactl
/// rejects non-ASCII in module arguments.
/// The TAP's two outputs to hardware: the hardware grid sends each to any sound cards.
pub use wbs_plugin::buses::{HW1, HW2, WBS_SINKS};

/// module-null-sink arguments for one WBS sink.
pub fn null_sink_args(name: &str, desc: &str, nick: &str) -> Vec<String> {
    vec![
        "module-null-sink".to_string(),
        format!("sink_name={name}"),
        format!("sink_properties='device.description=\"{desc}\" node.nick=\"{nick}\" device.icon_name=audio-card'"),
    ]
}

/// Tears down everything cleanly and rebuilds the entire WBS topology from zero.
pub fn full_restart() {
    let _g = state_lock();
    abandon_ship();
    thread::sleep(Duration::from_millis(250));

    d().routes = [true; 4];
    let phys = detect_physical_sink();
    d().physical_sink = phys.clone();

    for (name, desc, nick) in WBS_SINKS {
        load_module(&null_sink_args(name, desc, nick));
    }

    run_mut(&["pactl", "set-default-sink", "wbs_input_bus"]);
    move_all_streams_to("wbs_input_bus");
    refresh_sources();
    set_selected_source("wbs_input_bus.monitor");
    apply_routes();
    // HW 1 plays on the speakers until the hardware grid says otherwise
    let hw1 = format!("{HW1}.monitor");
    let hw = services::hardware();
    if !phys.is_empty() && !hw.has_link_from(&hw1) {
        if let Err(e) = hw.set(&hw1, &phys, true) {
            wbs_log::warn("hw", &format!("HW 1 -> {phys}: {e}"));
        }
    }

    // Start MXL Engine continuous ring buffers
    let sel = selected_source();
    if guard(|| services::mxl().start(&sel)).is_none() {
        println!("[WARN] MXL Engine start failed: panic");
    }

    println!("[WARD BECK TAP] Topology active. Source={sel} -> Speakers={phys}");
}

/// module-loopback argument lists for a selection (Python apply_routes steps 1-4).
/// Returns (loopbacks, speaker_split_muted_by_safeguard).
pub fn route_table(selected: &str, routes: [bool; 4], physical: &str) -> (Vec<Vec<String>>, bool) {
    let low = selected.to_lowercase();
    let source_is_mic = (low.contains("input") || low.contains("mic")) && !selected.ends_with(".monitor");
    // 40 ms: a 10 ms loopback asks PipeWire for a 128-sample (2.7 ms) graph cycle, which
    // underruns the HDMI sink whenever the desktop is busy. 40 ms lets the graph run at 512.
    let lb = |src: &str, sink: &str| {
        vec![
            "module-loopback".to_string(),
            format!("source={src}"),
            format!("sink={sink}"),
            "latency_msec=40".to_string(),
        ]
    };
    let mut out = Vec::new();
    let mut muted = false;
    // 1. Selected Source -> WBS Meter Tap
    if routes[0] {
        out.push(lb(selected, "wbs_meter_tap"));
    }
    // 2. Selected Source -> WBS Insert Bus
    if routes[1] {
        out.push(lb(selected, "wbs_insert_bus"));
    }
    // 3. WBS Insert Bus -> HW 1 (Speakers) (SAFEGUARDED); the hardware grid sends HW 1 to the cards
    let _ = physical;
    if routes[2] {
        if !source_is_mic {
            out.push(lb("wbs_insert_bus.monitor", HW1));
        } else {
            muted = true;
        }
    }
    // 4. WBS Insert Bus -> WBS Test Point
    if routes[3] {
        out.push(lb("wbs_insert_bus.monitor", "wbs_test_point"));
    }
    (out, muted)
}

/// Rebuilds WBS loopbacks using SELECTED_SOURCE as the patchbay input with acoustic feedback protection.
pub fn apply_routes() {
    let _g = state_lock();
    let (_, mods_out, _) = run_cmd(&["pactl", "list", "short", "modules"]);
    let managed = d().managed_modules.clone();
    let loopback_ids: Vec<i64> = tab_lines(&mods_out)
        .into_iter()
        .filter(|p| p.len() >= 2 && p[1] == "module-loopback" && py::isdigit(p[0]))
        .filter_map(|p| p[0].parse().ok())
        .filter(|mid| managed.contains(mid))
        .collect();
    for mid in loopback_ids.iter().rev() {
        run_mut(&["pactl", "unload-module", &mid.to_string()]);
        let mut g = d();
        if let Some(i) = g.managed_modules.iter().position(|m| m == mid) {
            g.managed_modules.remove(i);
        }
    }

    let (sel, routes, phys) = {
        let g = d();
        (g.selected_source.clone(), g.routes, g.physical_sink.clone())
    };
    if sel.starts_with("nmos:") || sel.starts_with("mxl:") {
        println!("[WARD BECK TAP] Network/MXL source tapped: {sel}");
        return;
    }

    let (loops, muted) = route_table(&sel, routes, &phys);
    for args in &loops {
        load_module(args);
    }
    if muted {
        println!("[SAFEGUARD] Physical speaker split muted: microphone/input source tapped to avoid acoustic feedback!");
    }

    // Update MXL edge ingestion source
    let _ = guard(|| services::mxl().set_source(&sel));
}

/// Reads raw PCM from wbs_meter_tap.monitor and calculates stereo VU ballistics.
pub fn meter_reader_loop() {
    const CHUNK_FRAMES: usize = 512;
    let mut buf = vec![0u8; CHUNK_FRAMES * 4]; // s16le stereo
    let (mut vu_l, mut vu_r) = (0.0f64, 0.0f64);
    while !is_shutting_down() {
        let spawned = Command::new("parec")
            .args([
                "--device=wbs_meter_tap.monitor",
                "--format=s16le",
                "--rate=24000",
                "--channels=2",
                "--latency-msec=35",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = spawned {
            if let Some(mut out) = child.stdout.take() {
                while !is_shutting_down() {
                    if out.read_exact(&mut buf).is_err() {
                        break;
                    }
                    let rms_l = chunk_rms(&buf, 2, 0);
                    let rms_r = chunk_rms(&buf, 2, 1);
                    // Simulated 300ms mechanical needle inertia filter
                    vu_l = 0.75 * vu_l + 0.25 * rms_l;
                    vu_r = 0.75 * vu_r + 0.25 * rms_r;
                    let mut db_l = 20.0 * vu_l.max(1e-5).log10();
                    let mut db_r = 20.0 * vu_r.max(1e-5).log10();
                    let mut vu_left = db_l + 18.0;
                    let mut vu_right = db_r + 18.0;

                    let _g = state_lock();
                    let mut g = d();
                    if g.selected_source.starts_with("mxl:") {
                        if let Some(sm) = g.source_meters.get(&g.selected_source) {
                            let f = |k: &str, dflt: f64| sm.get(k).and_then(|v| v.as_f64()).unwrap_or(dflt);
                            db_l = f("db", -60.0);
                            db_r = f("db", -60.0);
                            vu_left = f("vu", -42.0);
                            vu_right = f("vu", -42.0);
                        }
                    }
                    g.meter_levels = [
                        py::round(db_l, 1),
                        py::round(db_r, 1),
                        py::round(vu_left, 1),
                        py::round(vu_right, 1),
                    ];
                }
            }
            wbs_sys::kill(child.id(), wbs_sys::SIGTERM);
            let _ = child.wait();
        }
        thread::sleep(Duration::from_millis(500));
    }
}

// ---------------------------------------------------------------------------
// POD-33 test tone and ABB-1 Bit Buddy / ABP-1 Bit Spitter
// ---------------------------------------------------------------------------

fn spawn_pacat(rate: i64, stream_name: &str) -> std::io::Result<std::process::Child> {
    let mut cmd = Command::new("pacat");
    cmd.args([
        "--playback".to_string(),
        "--device=wbs_input_bus".to_string(),
        "--format=s16le".to_string(),
        format!("--rate={rate}"),
        "--channels=2".to_string(),
        "--latency-msec=20".to_string(),
        format!("--stream-name={stream_name}"),
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    wbs_sys::new_session(&mut cmd);
    cmd.spawn()
}

/// Python Popen.terminate(); Popen.kill().
fn term_kill(pid: u32) {
    kill_pid(pid, wbs_sys::SIGTERM);
    kill_pid(pid, wbs_sys::SIGKILL);
}

/// POST /api/test_tone start (Python start_test_tone(freq): int(freq) after the stop).
/// The POD-33 output the TAP plays: left frequency, an optional different right one
/// (split tones such as 1 kHz / 4 kHz), level and channel / phase.
#[derive(Clone, Debug, PartialEq)]
pub struct ToneSpec {
    pub freq: f64,
    pub freq_r: Option<f64>,
    pub level_db: f64,
    /// stereo | left | right | invert
    pub channel: String,
}

impl Default for ToneSpec {
    fn default() -> Self {
        ToneSpec { freq: 1000.0, freq_r: None, level_db: -18.0, channel: "stereo".into() }
    }
}

impl ToneSpec {
    /// From a /api/test_tone body: freq, freq_r, level_db, channel (missing = defaults).
    pub fn from_body(data: &Map<String, Value>) -> Result<Self, String> {
        let d = ToneSpec::default();
        Ok(ToneSpec {
            freq: py::int_of(data.get("freq").unwrap_or(&json!(1000)))? as f64,
            freq_r: data.get("freq_r").and_then(|v| v.as_f64()).filter(|f| *f > 0.0),
            level_db: data.get("level_db").and_then(|v| v.as_f64()).unwrap_or(d.level_db),
            channel: data.get("channel").and_then(|v| v.as_str()).unwrap_or("stereo").to_string(),
        })
    }
}

pub fn start_test_tone(spec: ToneSpec) -> Result<(), String> {
    stop_test_tone();
    // Note: Allows concurrent operation with ABP-1 Bit Spitter (Dual Flow Spitting)
    {
        let mut g = d();
        g.tone_freq = spec.freq as i64;
        g.tone = spec.clone();
        g.tone_active = true;
    }
    let _ = guard(|| services::mxl().start_generator("tone"));
    thread::spawn(move || tone_stream_worker(spec));
    Ok(())
}

/// s16le stereo frames of a ToneSpec; `phase` is (left, right).
pub fn tone_spec_chunk(spec: &ToneSpec, phase: &mut (f64, f64), chunk: usize) -> Vec<u8> {
    let two_pi = 2.0 * std::f64::consts::PI;
    let amp = if spec.level_db <= -90.0 { 0.0 } else { 10f64.powf(spec.level_db / 20.0) };
    let (sl, sr) = (two_pi * spec.freq / 48000.0, two_pi * spec.freq_r.unwrap_or(spec.freq) / 48000.0);
    let mut raw = Vec::with_capacity(chunk * 4);
    for _ in 0..chunk {
        let mut l = if spec.freq > 0.0 { amp * phase.0.sin() } else { 0.0 };
        let mut r = if spec.freq > 0.0 { amp * phase.1.sin() } else { 0.0 };
        match spec.channel.as_str() {
            "left" => r = 0.0,
            "right" => l = 0.0,
            "invert" => r = -r,
            _ => {}
        }
        raw.extend_from_slice(&((l * 32767.0) as i16).to_le_bytes());
        raw.extend_from_slice(&((r * 32767.0) as i16).to_le_bytes());
        phase.0 = (phase.0 + sl) % two_pi;
        phase.1 = (phase.1 + sr) % two_pi;
    }
    raw
}

pub fn stop_test_tone() {
    let mut g = d();
    g.tone_active = false;
    if let Some(pid) = g.tone_pid.take() {
        term_kill(pid);
    }
}

/// s16le stereo frames of a -18 dBFS sine (Python _tone_stream_worker chunk).
pub fn tone_chunk(phase: &mut f64, step: f64, chunk: usize) -> Vec<u8> {
    let amp = 32767.0 * 0.12589; // -18 dBFS reference level
    let mut raw = Vec::with_capacity(chunk * 4);
    for _ in 0..chunk {
        let v = (amp * phase.sin()) as i16;
        raw.extend_from_slice(&v.to_le_bytes());
        raw.extend_from_slice(&v.to_le_bytes());
        *phase = (*phase + step) % (2.0 * std::f64::consts::PI);
    }
    raw
}

fn tone_stream_worker(spec: ToneSpec) {
    let freq = spec.freq;
    if dry_run() {
        println!("[DRY RUN] pacat --playback --device=wbs_input_bus (Ward-Beck Test Tone {freq} Hz)");
        return;
    }
    let mut child = match spawn_pacat(48000, "Ward-Beck Test Tone") {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[Ward-Beck Tone] Error: {e}");
            return;
        }
    };
    let pid = child.id();
    d().tone_pid = Some(pid);
    let mut p = (0.0f64, 0.0f64);
    let mut stdin = child.stdin.take();
    loop {
        let active = {
            let g = d();
            g.tone_active && g.tone_pid == Some(pid)
        };
        if !active || !matches!(child.try_wait(), Ok(None)) {
            break;
        }
        let raw = tone_spec_chunk(&spec, &mut p, 256);
        match stdin.as_mut() {
            Some(s) => {
                if s.write_all(&raw).and_then(|_| s.flush()).is_err() {
                    break;
                }
            }
            None => break,
        }
    }
    {
        let mut g = d();
        if g.tone_pid == Some(pid) {
            kill_pid(pid, wbs_sys::SIGTERM);
            g.tone_pid = None;
        }
    }
    drop(stdin);
    let _ = child.wait();
}

/// POST /api/bitbuddy start: `cfg` is the whole Bit Spitter setting (pattern, level_db,
/// tone_hz, ch_b, ext, ext_locked, fault, sample_rate, pro_mode, audio_data, validity).
pub fn start_bit_buddy(cfg: Map<String, Value>) -> Result<(), String> {
    stop_bit_buddy();
    // Note: Allows concurrent operation with POD-33 Tone Generator (Dual Flow Spitting)
    let sr = py::int_of(cfg.get("sample_rate").unwrap_or(&json!(48000)))?;
    d().bb_active = true;
    let _ = guard(|| services::mxl().start_generator("bit_spitter"));
    {
        let mut m = cfg;
        m.insert("sample_rate".into(), json!(sr));
        d().bb_config = m;
    }
    thread::spawn(bitbuddy_worker);
    Ok(())
}

pub fn stop_bit_buddy() {
    let mut g = d();
    g.bb_active = false;
    if let Some(pid) = g.bb_pid.take() {
        term_kill(pid);
    }
}

/// The looped file the Bit Spitter plays (pattern "file"): name and 48 kHz stereo PCM.
fn bb_file() -> MutexGuard<'static, Option<(String, SpitterFile)>> {
    static FILE: Mutex<Option<(String, SpitterFile)>> = Mutex::new(None);
    FILE.lock().unwrap_or_else(|e| e.into_inner())
}

/// POST /api/bitbuddy/file {name, rate, pcm_b64: s16le stereo} or {clear: true}.
pub fn set_bit_buddy_file(data: &Map<String, Value>) -> Result<Value, String> {
    if data.get("clear").map(py::truthy).unwrap_or(false) {
        *bb_file() = None;
        return Ok(json!({"file": null}));
    }
    let b64 = data.get("pcm_b64").and_then(|v| v.as_str()).ok_or("pcm_b64 missing")?;
    let raw = base64_decode(b64)?;
    let pcm: Vec<f32> = raw.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect();
    let rate = data.get("rate").and_then(|v| v.as_f64()).unwrap_or(48000.0);
    let name = data.get("name").and_then(|v| v.as_str()).unwrap_or("file").to_string();
    let secs = pcm.len() as f64 / 2.0 / rate;
    wbs_log::info("bitbuddy", &format!("file loaded: {name} ({secs:.1} s)"));
    *bb_file() = Some((name.clone(), SpitterFile { pcm: std::sync::Arc::new(pcm), rate }));
    Ok(json!({"file": name, "seconds": secs}))
}

fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        acc = (acc << 6) | val(c).ok_or("bad base64")?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

/// The synth settings from the stored config.
fn spitter_params(cfg: &Map<String, Value>) -> SpitterParams {
    let s = |k: &str, d: &str| cfg.get(k).and_then(|v| v.as_str()).unwrap_or(d).to_string();
    let b = |k: &str| cfg.get(k).map(py::truthy).unwrap_or(false);
    let ch_b = s("ch_b", "on");
    let pattern = s("pattern", "sweep");
    SpitterParams {
        file: if pattern == "file" { bb_file().as_ref().map(|(_, f)| f.clone()) } else { None },
        pattern,
        level: match cfg.get("level_db") {
            Some(Value::Null) => None,
            Some(v) => v.as_f64().or(Some(-20.0)),
            None => Some(-20.0),
        },
        tone: cfg.get("tone_hz").and_then(|v| v.as_f64()).unwrap_or(1000.0),
        cycle: ch_b.starts_with("cycle"),
        invert: ch_b.ends_with("invert"),
        b_off: ch_b == "off",
        muted: b("ext") && !b("ext_locked"),
        fault: s("fault", "clean"),
    }
}

/// One chunk of s16le stereo frames.
pub fn spitter_chunk(gen: &mut SpitterSynth, frames: usize) -> Vec<u8> {
    let mut raw = Vec::with_capacity(frames * 4);
    for _ in 0..frames {
        let (l, r) = gen.next();
        let q = |v: f64| ((v.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes();
        raw.extend_from_slice(&q(l));
        raw.extend_from_slice(&q(r));
    }
    raw
}

fn bitbuddy_worker() {
    let cfg = bitbuddy_config();
    let sr = cfg.get("sample_rate").and_then(|v| v.as_i64()).unwrap_or(48000);
    let params = spitter_params(&cfg);
    if dry_run() {
        println!("[DRY RUN] pacat --playback --device=wbs_input_bus (Ward-Beck ABS-1 Bit Spitter {} @ {sr})", params.pattern);
        return;
    }
    let mut child = match spawn_pacat(sr, "Ward-Beck ABS-1 Bit Spitter") {
        Ok(c) => c,
        Err(_) => return, // Python: OSError is swallowed
    };
    let pid = child.id();
    d().bb_pid = Some(pid);
    let mut stdin = child.stdin.take();
    if sr <= 0 {
        eprintln!("[WARD BECK TAP] Bit Buddy Error: float division by zero");
    } else {
        let mut gen = SpitterSynth::new(params, sr as f64);
        loop {
            let active = {
                let g = d();
                g.bb_active && g.bb_pid == Some(pid)
            };
            if !active || !matches!(child.try_wait(), Ok(None)) {
                break;
            }
            let raw = spitter_chunk(&mut gen, 256);
            match stdin.as_mut() {
                Some(s) => {
                    if s.write_all(&raw).and_then(|_| s.flush()).is_err() {
                        break;
                    }
                }
                None => break,
            }
        }
    }
    {
        let mut g = d();
        if g.bb_pid == Some(pid) {
            kill_pid(pid, wbs_sys::SIGTERM);
            g.bb_pid = None;
        }
    }
    drop(stdin);
    let _ = child.wait();
}

// ---------------------------------------------------------------------------
// Cached pipeline report (Python get_cached_pipeline(engine, max_age=3.0))
// ---------------------------------------------------------------------------
pub fn get_cached_pipeline() -> Value {
    static CACHE: Mutex<Option<(Value, f64)>> = Mutex::new(None);
    let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let now = now_f64();
    let stale = match &*c {
        Some((v, t)) => now - *t > 3.0 || !py::truthy(v),
        None => true,
    };
    if stale {
        if let Some(v) = guard(|| services::pipeline().inspect_full()) {
            *c = Some((v, now));
        }
    }
    c.as_ref().map(|(v, _)| v.clone()).unwrap_or_else(|| Value::Object(Map::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn py_float_repr() {
        assert_eq!(py::repr_float(-20.0), "-20.0");
        assert_eq!(py::repr_float(1795398043.5718203), "1795398043.5718203");
        assert_eq!(py::repr_float(1e-5), "1e-05");
        assert_eq!(py::repr_float(0.0001), "0.0001");
        assert_eq!(py::repr_float(1e16), "1e+16");
        assert_eq!(py::repr_float(1e15), "1000000000000000.0");
        assert_eq!(py::repr_float(-0.10902614146471024), "-0.10902614146471024");
        assert_eq!(py::repr_float(1.5e300), "1.5e+300");
        assert_eq!(py::repr_float(2683.17), "2683.17");
        assert_eq!(py::repr_float(0.0), "0.0");
        assert_eq!(py::repr_float(123456789012345678.0), "1.2345678901234568e+17");
    }

    #[test]
    fn py_dumps_matches_python() {
        let v: Value = serde_json::from_str(
            r#"{"a": [1, 2.5, "x\u00e9\ud83d\udce1\"\n"], "b": {}, "c": [], "d": null, "e": true, "f": -100.0}"#,
        )
        .unwrap();
        assert_eq!(py::dumps(&v), py::dumps_local(&v));
        assert_eq!(
            py::dumps(&v),
            r#"{"a": [1, 2.5, "x\u00e9\ud83d\udce1\"\n"], "b": {}, "c": [], "d": null, "e": true, "f": -100.0}"#
        );
    }

    #[test]
    fn py_round_ties_even() {
        assert_eq!(py::round(0.25, 1), 0.2);
        assert_eq!(py::round(0.35, 1), 0.3); // 0.35 is below the tie in binary
        assert_eq!(py::round(2.675, 2), 2.67);
        assert_eq!(py::round(-99.95, 1), -100.0); // binary -99.9500000000000028
        assert_eq!(py::round(3.57182031, 4), 3.5718);
    }

    #[test]
    fn py_int_parse() {
        assert_eq!(py::parse_int(" 360 ").unwrap(), 360);
        assert_eq!(py::parse_int("-1").unwrap(), -1);
        assert_eq!(py::parse_int("1_000").unwrap(), 1000);
        assert!(py::parse_int("1.5").is_err());
        assert!(py::parse_int("").is_err());
        assert_eq!(py::int_of(&json!(1000.9)).unwrap(), 1000);
        assert_eq!(py::int_of(&json!("48000")).unwrap(), 48000);
        assert!(py::int_of(&json!(null)).is_err());
        assert_eq!(py::float_of(&json!("-12.5")).unwrap(), -12.5);
    }

    #[test]
    fn py_str_of() {
        assert_eq!(py::str_of(&json!(null)), "None");
        assert_eq!(py::str_of(&json!(8)), "8");
        assert_eq!(py::str_of(&json!(["8"])), "['8']");
        assert_eq!(py::str_of(&json!(true)), "True");
    }

    #[test]
    fn virtual_sinks() {
        assert!(is_virtual_sink(""));
        assert!(is_virtual_sink("wbs_input_bus"));
        assert!(is_virtual_sink("alsa_output.platform-snd_aloop.0.analog-stereo"));
        assert!(is_virtual_sink("XTM_Null_Sink"));
        assert!(is_virtual_sink("input.pw-loopback-123"));
        assert!(!is_virtual_sink("alsa_output.pci-0000_01_00.1.hdmi-stereo-extra1.9"));
        assert!(!is_virtual_sink("bluez_output.AA_BB.1"));
    }

    #[test]
    fn physical_sink_priorities() {
        let out = "1\twbs_input_bus\tmodule-null-sink.c\ts16le 2ch 48000Hz\tIDLE\n\
                   2\talsa_output.platform-snd_aloop.0.analog-stereo\tPipeWire\ts32le 2ch 48000Hz\tIDLE\n\
                   3\tbluez_output.AA.1\tPipeWire\ts16le 2ch 48000Hz\tIDLE\n\
                   4\talsa_output.pci-0000_01_00.1.hdmi-stereo\tPipeWire\ts32le 2ch 48000Hz\tIDLE";
        assert_eq!(pick_physical_sink(out, || unreachable!()), "alsa_output.pci-0000_01_00.1.hdmi-stereo");
        let out2 = "1\twbs_input_bus\tx\n3\tbluez_output.AA.1\tx";
        assert_eq!(pick_physical_sink(out2, || unreachable!()), "bluez_output.AA.1");
        let out3 = "1\twbs_input_bus\tx\n2\tmy_speakers\tx";
        assert_eq!(pick_physical_sink(out3, || "wbs_input_bus".into()), "my_speakers");
        assert_eq!(pick_physical_sink(out3, || "other_real".into()), "other_real");
        assert_eq!(pick_physical_sink("1\twbs_a\tx", String::new), "wbs_a");
        assert_eq!(pick_physical_sink("", String::new), "");
    }

    #[test]
    fn pactl_sources_parse() {
        let out = "Source #52\n\tState: SUSPENDED\n\tName: wbs_input_bus.monitor\n\tDescription: Monitor of WBS_Input_Bus\n\
                   \tDriver: PipeWire\nSource #60\n\tName: XTM_Audio_Source\n\tDriver: x\n";
        let s = parse_pactl_sources(out);
        assert_eq!(
            py::dumps(&Value::Array(s)),
            r#"[{"name": "wbs_input_bus.monitor", "description": "Monitor of WBS_Input_Bus"}, {"name": "XTM_Audio_Source", "description": "XTM_Audio_Source"}]"#
        );
    }

    #[test]
    fn nmos_source_shape() {
        let nodes: Value = serde_json::from_str(
            r#"[{"id": "n", "label": "uNet2500 (Archwave)", "hostname": "WBS-32me", "ip": "44.44.44.158",
                "flows": [{"id": "00010001-0000-1000-8004-0007f54206a2", "label": "32ME-STREAM-1",
                "tags": {"channels": ["8"], "clockRate": ["48000"], "encodingName": ["L24"]}}]}]"#,
        )
        .unwrap();
        let s = nmos_sources(nodes.as_array().unwrap());
        assert_eq!(
            py::dumps(&s[0]),
            r#"{"name": "nmos:44.44.44.158:00010001-0000-1000-8004-0007f54206a2", "description": "\ud83d\udce1 NMOS: WBS-32me \u2014 32ME-STREAM-1 (8ch L24 @ 48000Hz)", "nmos": true, "node_ip": "44.44.44.158", "node_label": "WBS-32me", "flow_id": "00010001-0000-1000-8004-0007f54206a2", "flow_label": "32ME-STREAM-1", "channels": "8", "rate": "48000"}"#
        );
    }

    #[test]
    fn aoip_source_shape() {
        let a: Value = serde_json::from_str(
            r#"{"dante": {"devices": [{"name": "Metro16-DANTE2AVB", "ip": "44.44.44.184", "engine": "Audinate DCM"}]},
                "ravenna": {"streams": [{"name": "32ME-STREAM-1", "ip": "44.44.44.158", "port": 554, "url": "u"}]},
                "aes67_sap": {"active_announcements": [{"session_name": "S", "multicast_ip": "239.1.1.1", "port": 5004}]}}"#,
        )
        .unwrap();
        let s = aoip_sources(&a);
        assert_eq!(s.len(), 4);
        assert_eq!(
            py::dumps(&s[0]),
            r#"{"name": "dante:Metro16-DANTE2AVB:Ch1-Ch2", "description": "\ud83c\udfb5 DANTE: Metro16-DANTE2AVB \u2014 Ch 1 & 2 (Audinate DCM @ 48kHz L24)", "dante": true, "device": "Metro16-DANTE2AVB", "ip": "44.44.44.184"}"#
        );
        assert_eq!(
            s[2]["description"],
            "\u{1F4FB} RAVENNA: 32ME-STREAM-1 \u{2014} rtsp://44.44.44.158:554/32ME-STREAM-1 (Archwave uNet2500)"
        );
        assert_eq!(s[3]["name"], "aes67:S");
    }

    #[test]
    fn mxl_source_shape() {
        let doms: Value = serde_json::from_str(
            r#"{"own": {"flows": [{"flow_id": "x", "format": "urn:x-nmos:format:audio"}]},
                "Ward-Beck Test Signals": {"flows": [{"flow_id": "25030044-0440-4416-8000-000000000009", "label": "L", "description": "D", "format": "urn:x-nmos:format:audio"},
                                                     {"flow_id": "v", "format": "urn:x-nmos:format:video"}]},
                "exercise-4": {"flows": [{"id": "abcdefghij", "channel_count": 2}]}}"#,
        )
        .unwrap();
        let s = mxl_sources(&doms, "own");
        assert_eq!(s.len(), 2);
        assert_eq!(
            py::dumps(&s[0]),
            r#"{"name": "mxl:Ward-Beck Test Signals:25030044-0440-4416-8000-000000000009", "description": "\u26a1 WBS TEST SIGNAL: L \u2014 D", "mxl": true, "domain": "Ward-Beck Test Signals", "flow_id": "25030044-0440-4416-8000-000000000009", "flow_label": "L", "is_wbs_test_signal": true}"#
        );
        assert_eq!(s[1]["description"], "\u{1F39B}\u{FE0F} CBC MXL (exercise-4): abcdefgh \u{2014} ");
    }

    #[test]
    fn routes_and_safeguard() {
        let (l, muted) = route_table("wbs_input_bus.monitor", [true; 4], "alsa_output.x");
        assert!(!muted);
        assert_eq!(l.len(), 4);
        assert_eq!(l[0], ["module-loopback", "source=wbs_input_bus.monitor", "sink=wbs_meter_tap", "latency_msec=40"]);
        assert_eq!(l[2], ["module-loopback", "source=wbs_insert_bus.monitor", "sink=wbs_hw1", "latency_msec=40"]);
        assert_eq!(l[3], ["module-loopback", "source=wbs_insert_bus.monitor", "sink=wbs_test_point", "latency_msec=40"]);
        let (l, muted) = route_table("alsa_input.usb-mic", [true; 4], "alsa_output.x");
        assert!(muted);
        assert_eq!(l.len(), 3);
        let (l, _) = route_table("x.monitor", [false, true, true, false], "");
        assert_eq!(l.len(), 2); // HW 1 is always there, a speaker card or not
    }

    #[test]
    fn module_and_ps_parsing() {
        let mods = "536870913\tmodule-null-sink\tsink_name=wbs_input_bus sink_properties=device.description=WBS_Input_Bus\n\
                    7\tmodule-loopback\tsource=wbs_input_bus.monitor sink=wbs_meter_tap\n9\tmodule-x\t";
        assert_eq!(wbs_module_ids(mods), vec![536870913, 7]);
        let ps = "    1     0 /sbin/init\n  100     1 parec --device=x\n  200   300 pw-loopback --name XTM_Audio_Bridge\n\
                  201   300 pw-loopback -n other\n  300     1 /usr/lib/systemd/systemd --user\n  400   300 pacat x\n  500   999 parec y\n  999     1 me";
        let rows = parse_ps(ps);
        assert_eq!(rows.len(), 8);
        let s: Vec<u32> = stray_processes(&rows, 999).into_iter().map(|r| r.0).collect();
        assert_eq!(s, vec![100, 201, 400]);
    }

    #[test]
    fn generators() {
        let mut p = 0.0;
        let c = tone_chunk(&mut p, 2.0 * std::f64::consts::PI * 1000.0 / 48000.0, 4);
        let s: Vec<i16> = c.chunks(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        assert_eq!(s, vec![0, 0, 538, 538, 1067, 1067, 1578, 1578]);
        let mut cfg = default_bb_config();
        cfg.insert("pattern".into(), json!("tone"));
        cfg.insert("level_db".into(), json!(-20));
        cfg.insert("ch_b".into(), json!("invert"));
        let mut g = SpitterSynth::new(spitter_params(&cfg), 48000.0);
        let c = spitter_chunk(&mut g, 3);
        let s: Vec<i16> = c.chunks(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        assert_eq!(s[2], -s[3], "channel B inverted");
        assert!(s[4] > 0 && s[4] < 3300);
        assert_eq!(base64_decode("AAEC/w==").unwrap(), vec![0, 1, 2, 255]);
        let spec = ToneSpec { freq: 1000.0, freq_r: Some(4000.0), level_db: -20.0, channel: "stereo".into() };
        let c = tone_spec_chunk(&spec, &mut (0.0, 0.0), 3);
        let s: Vec<i16> = c.chunks(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        assert_eq!(s[2], (0.1 * (2.0 * std::f64::consts::PI / 48.0).sin() * 32767.0) as i16);
        assert_eq!(s[3], (0.1 * (2.0 * std::f64::consts::PI / 12.0).sin() * 32767.0) as i16, "right at 4 kHz");
    }

    #[test]
    fn rlock_reentrant() {
        let _a = state_lock();
        let _b = state_lock();
    }
}

// ---------------------------------------------------------------------------
// Client watchdog: a tone or Bit Spitter left on by a page that was closed or
// crashed stops by itself. Every browser request (it carries Sec-Fetch-Mode)
// and every POST /api/heartbeat counts as a check-in. XTM polls every 4.5 s
// and the TAP page every 4 s, so an open page always keeps the signal alive.
// WBS_CLIENT_TIMEOUT=<seconds> changes the limit; 0 turns the watchdog off.
// ---------------------------------------------------------------------------

static LAST_CLIENT: Mutex<Option<Instant>> = Mutex::new(None);

/// Record that a client is still there.
pub fn client_checkin() {
    if let Ok(mut g) = LAST_CLIENT.lock() {
        *g = Some(Instant::now());
    }
}

/// Seconds since the last check-in (None before the first one).
pub fn client_idle_secs() -> Option<f64> {
    LAST_CLIENT.lock().ok().and_then(|g| g.map(|t| t.elapsed().as_secs_f64()))
}

pub fn client_timeout_secs() -> f64 {
    std::env::var("WBS_CLIENT_TIMEOUT").ok().and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(12.0)
}

/// True when a signal is on and no client has checked in within `limit`.
pub fn watchdog_expired(signal_on: bool, idle: Option<f64>, limit: f64) -> bool {
    signal_on && limit > 0.0 && idle.map_or(true, |s| s > limit)
}

pub fn client_watchdog_loop() {
    while !is_shutting_down() {
        thread::sleep(Duration::from_secs(1));
        let limit = client_timeout_secs();
        let (tone, bb) = (test_tone_active(), bitbuddy_active());
        if !watchdog_expired(tone || bb, client_idle_secs(), limit) {
            continue;
        }
        let _g = state_lock();
        if test_tone_active() {
            println!("[WARD BECK TAP] No client for {limit:.0} s: stopping the test tone");
            stop_test_tone();
        }
        if bitbuddy_active() {
            println!("[WARD BECK TAP] No client for {limit:.0} s: stopping the Bit Spitter");
            stop_bit_buddy();
        }
    }
}

#[cfg(test)]
mod watchdog_tests {
    use super::watchdog_expired;

    #[test]
    fn expires_only_when_on_and_idle() {
        assert!(!watchdog_expired(false, Some(100.0), 12.0));
        assert!(!watchdog_expired(true, Some(3.0), 12.0));
        assert!(watchdog_expired(true, Some(12.5), 12.0));
        assert!(watchdog_expired(true, None, 12.0));
        assert!(!watchdog_expired(true, None, 0.0));
    }
}
