// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.com - Ward-Beck.ca). All rights reserved.
//! Port of pipeline_inspector.py: the 4-layer Linux audio + AoIP pipeline report.
//!   1. Kernel & Hardware Layer: ALSA cards, PCM devices, mixer controls (aplay/amixer).
//!   2. Graph & Session Layer: PipeWire graph & WirePlumber session manager (wpctl/pw-dump).
//!   3. Shared-Memory Layer: EBU DMF / MXL (/dev/shm/mxl) Float32 ring buffers.
//!   4. AoIP Network Layer: Dante, Ravenna, AES67 SAP, NMOS IS-04 (avahi-browse).
//! CONTRACT: keep these pub signatures.

// Plugged into the host: routes, services and start-up (plugin.rs).
pub mod plugin;
pub use plugin::plug;

use serde_json::{json, Map, Value};
use std::sync::Mutex;
use std::time::Duration;

use wbs_common::{now_f64, run_cmd_full, shm_base};

// ---------------------------------------------------------------------------
// Python-compatible helpers (string ones live in wbs_pyshim::text)
// ---------------------------------------------------------------------------

pub use wbs_pyshim::text::{py_is_space, py_splitlines, py_strip};

/// Python round(x, n) for floats: correctly rounded, ties-to-even on the exact
/// binary value (Rust's `{:.n}` formatting does the same).
pub(crate) fn py_round(x: f64, n: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{:.*}", n, x).parse().unwrap_or(x)
}

/// Python pipeline_inspector.run_cmd: (returncode, stdout.strip()); -1 on failure/timeout.
fn run(args: &[&str], timeout: f64) -> (i32, String) {
    match run_cmd_full(args, Duration::from_secs_f64(timeout)) {
        Some((out, code)) => (code, py_strip(&out).to_string()),
        None => (-1, String::new()),
    }
}

/// Content of the first `[...]` group whose inside satisfies `pred`
/// (equivalent to re.search(r"\[(PATTERN)\]") for bracket-free patterns).
fn first_bracket(s: &str, pred: impl Fn(&str) -> bool) -> Option<String> {
    let mut rest = s;
    while let Some(i) = rest.find('[') {
        let after = &rest[i + 1..];
        if let Some(j) = after.find(']') {
            let inner = &after[..j];
            if !inner.contains('[') && pred(inner) {
                return Some(inner.to_string());
            }
        }
        rest = after;
    }
    None
}

fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

/// `\d+%`
fn is_percent(s: &str) -> bool {
    s.strip_suffix('%').map(all_digits).unwrap_or(false)
}

/// `-?\d+\.\d+dB`
fn is_db(s: &str) -> bool {
    let Some(body) = s.strip_suffix("dB") else { return false };
    let body = body.strip_prefix('-').unwrap_or(body);
    match body.split_once('.') {
        Some((a, b)) => all_digits(a) && all_digits(b),
        None => false,
    }
}

/// First `'name'` on a line (re.search(r"'([^']+)'")).
fn first_quoted(line: &str) -> Option<String> {
    let mut rest = line;
    while let Some(i) = rest.find('\'') {
        let after = &rest[i + 1..];
        match after.find('\'') {
            Some(0) => rest = after,
            Some(j) => return Some(after[..j].to_string()),
            None => return None,
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Layer 1: ALSA
// ---------------------------------------------------------------------------

pub fn inspect_alsa() -> Value {
    let mut cards: Vec<Value> = Vec::new();
    let (code, out) = run(&["aplay", "-l"], 2.0);
    if code == 0 {
        for line in py_splitlines(&out) {
            let line = py_strip(line);
            if line.starts_with("card ") {
                cards.push(Value::String(line.to_string()));
            }
        }
    }

    let mut mixer_controls: Vec<Value> = Vec::new();
    let (code, sctrls) = run(&["amixer", "scontrols"], 2.0);
    if code == 0 {
        for line in py_splitlines(&sctrls) {
            if let Some(cname) = first_quoted(line) {
                let (_, sc) = run(&["amixer", "sget", &cname], 2.0);
                let na = || "N/A".to_string();
                mixer_controls.push(json!({
                    "name": cname,
                    "volume": first_bracket(&sc, is_percent).unwrap_or_else(na),
                    "db": first_bracket(&sc, is_db).unwrap_or_else(na),
                    "state": first_bracket(&sc, |s| s == "on" || s == "off").unwrap_or_else(na),
                }));
            }
        }
    }

    let pcm = std::path::Path::new("/dev/snd/pcmC0D0c").exists() || std::path::Path::new("/proc/asound").exists();
    json!({
        "status": if cards.is_empty() { "no_cards" } else { "active" },
        "card_count": cards.len(),
        "cards": cards,
        "mixer_controls": mixer_controls,
        "pcm_capture_available": pcm,
    })
}

// ---------------------------------------------------------------------------
// Layer 2: PipeWire / WirePlumber
// ---------------------------------------------------------------------------

/// re.sub(r"^[│\s*├─└]+\s*", "", line).strip()
fn clean_tree_line(line: &str) -> &str {
    let t = line.trim_start_matches(|c: char| py_is_space(c) || matches!(c, '│' | '*' | '├' | '─' | '└'));
    py_strip(t)
}

/// Parse `wpctl status` into (devices, sinks, sources, streams), exactly like the Python.
pub(crate) fn parse_wpctl(out: &str) -> [Vec<String>; 4] {
    let mut lists: [Vec<String>; 4] = Default::default();
    let mut section: Option<usize> = None;
    for line in py_splitlines(out) {
        let s = py_strip(line);
        if s.contains("Devices:") {
            section = Some(0);
            continue;
        } else if s.contains("Sinks:") {
            section = Some(1);
            continue;
        } else if s.contains("Sources:") {
            section = Some(2);
            continue;
        } else if s.contains("Streams:") {
            section = Some(3);
            continue;
        } else if s.starts_with("Video") || s.starts_with("Settings") {
            section = None;
            continue;
        }
        if let Some(idx) = section {
            if !s.is_empty() && !s.starts_with('├') && !s.starts_with('└') && !s.starts_with('│') {
                let cleaned = clean_tree_line(line);
                if cleaned.is_empty() {
                    continue;
                }
                lists[idx].push(cleaned.to_string());
            }
        }
    }
    lists
}

pub fn inspect_pipewire() -> Value {
    let (code, wp_out) = run(&["wpctl", "status"], 2.0);
    let [devices, sinks, sources, mut streams] =
        if code == 0 { parse_wpctl(&wp_out) } else { Default::default() };

    let mut graph_quantum = json!(256);
    let graph_rate = 48000;
    let (code, dump) = run(&["pw-dump", "Node"], 2.0);
    if code == 0 && !py_strip(&dump).is_empty() {
        if let Ok(Value::Array(nodes)) = serde_json::from_str::<Value>(&dump) {
            for node in &nodes {
                if let Some(q) = node.get("info").and_then(|i| i.get("props")).and_then(|p| p.get("clock.quantum-limit")) {
                    graph_quantum = q.clone();
                }
            }
        }
    }
    let q = graph_quantum.as_f64().unwrap_or(256.0);
    streams.truncate(12);

    json!({
        "status": if !sinks.is_empty() || !sources.is_empty() { "running" } else { "idle" },
        "server": "PipeWire 1.0 (with WirePlumber)",
        "graph_sample_rate": graph_rate,
        "graph_quantum": graph_quantum,
        "graph_quantum_ms": py_round((q / graph_rate as f64) * 1000.0, 2),
        "devices": devices,
        "sinks": sinks,
        "sources": sources,
        "streams": streams,
    })
}

// ---------------------------------------------------------------------------
// Layer 4: AoIP
// ---------------------------------------------------------------------------

static AOIP_CACHE: Mutex<Option<(Value, f64)>> = Mutex::new(None);

/// re.search(KEY + r'([^"]+)', txt) -> first occurrence followed by >=1 non-quote char.
fn txt_value(txt: &str, key: &str) -> Option<String> {
    let mut pos = 0;
    while let Some(i) = txt[pos..].find(key) {
        let start = pos + i + key.len();
        let v: &str = match txt[start..].find('"') {
            Some(j) => &txt[start..start + j],
            None => &txt[start..],
        };
        if !v.is_empty() {
            return Some(v.to_string());
        }
        pos = pos + i + 1;
    }
    None
}

/// re.search(KEY + r'(\d+)', txt) -> int
fn txt_int(txt: &str, key: &str) -> Option<i64> {
    let mut pos = 0;
    while let Some(i) = txt[pos..].find(key) {
        let start = pos + i + key.len();
        let d: String = txt[start..].chars().take_while(|c| c.is_ascii_digit()).collect();
        if !d.is_empty() {
            return d.parse().ok();
        }
        pos = pos + i + 1;
    }
    None
}

/// One resolved avahi-browse -p record ("=" line with >= 9 fields).
struct AvahiRec {
    parts: Vec<String>,
    port: i64,
}

impl AvahiRec {
    fn txt(&self) -> String {
        if self.parts.len() > 9 { self.parts[9..].join(";") } else { String::new() }
    }
}

/// Parse avahi-browse -r -t -p output into resolved records. A record whose port is
/// not an integer is skipped (the Python would raise ValueError).
pub(crate) fn parse_avahi(out: &str) -> Vec<(Vec<String>, i64)> {
    let mut recs = Vec::new();
    for line in py_splitlines(out) {
        let parts: Vec<String> = line.split(';').map(str::to_string).collect();
        if parts.len() >= 9 && parts[0] == "=" {
            if let Ok(port) = py_strip(&parts[8]).parse::<i64>() {
                recs.push((parts, port));
            }
        }
    }
    recs
}

fn browse(service: &str) -> Vec<AvahiRec> {
    let (code, out) = run(&["avahi-browse", "-r", "-t", "-p", service], 1.8);
    if code != 0 {
        return Vec::new();
    }
    parse_avahi(&out).into_iter().map(|(parts, port)| AvahiRec { parts, port }).collect()
}

pub fn inspect_aoip(use_cache: bool) -> Value {
    let now = now_f64();
    if use_cache {
        if let Some((v, t)) = AOIP_CACHE.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let non_empty = v.as_object().map(|o| !o.is_empty()).unwrap_or(false);
            if non_empty && now - t < 15.0 {
                return v.clone();
            }
        }
    }

    let mut dante_devices: Vec<Value> = Vec::new();
    let mut dante_channels: Vec<Value> = Vec::new();
    let mut ravenna_streams: Vec<Value> = Vec::new();
    let mut nmos_nodes: Vec<Value> = Vec::new();

    // 1. Dante devices
    for r in browse("_netaudio-arc._udp") {
        let txt = r.txt();
        dante_devices.push(json!({
            "name": r.parts[3],
            "host": r.parts[6],
            "ip": r.parts[7],
            "port": r.port,
            "manufacturer": txt_value(&txt, "mf=").unwrap_or_else(|| "Audinate / OEM".into()),
            "engine": txt_value(&txt, "router_info=").unwrap_or_else(|| "DCM".into()),
            "version": txt_value(&txt, "router_vers=").unwrap_or_else(|| "4.0".into()),
        }));
    }

    // Dante channels
    for r in browse("_netaudio-chan._udp") {
        let txt = r.txt();
        dante_channels.push(json!({
            "channel": r.parts[3].replace("\\064", "@"),
            "sample_rate": txt_int(&txt, "rate=").unwrap_or(48000),
            "bit_depth": txt_int(&txt, "enc=").unwrap_or(24),
            "ip": r.parts[7],
        }));
    }

    // 2. Ravenna / RTSP
    for r in browse("_rtsp._tcp") {
        let (sname, ip) = (&r.parts[3], &r.parts[7]);
        ravenna_streams.push(json!({
            "name": sname,
            "host": r.parts[6],
            "ip": ip,
            "port": r.port,
            "url": format!("rtsp://{}:{}/{}", ip, r.port, sname),
            "standard": "RAVENNA / AES67",
        }));
    }

    // 3. NMOS IS-04 nodes
    for r in browse("_nmos-node._tcp") {
        let ip = &r.parts[7];
        nmos_nodes.push(json!({
            "name": r.parts[3],
            "host": r.parts[6],
            "ip": ip,
            "port": r.port,
            "api": format!("http://{}:{}/x-nmos/node/v1.3/self", ip, r.port),
        }));
    }

    let has = |list: &Vec<Value>, name: &str| list.iter().any(|d| d["name"] == name);

    // Fallback to known hardware targets if not resolved via avahi
    if !has(&dante_devices, "Metro16-DANTE2AVB") {
        dante_devices.push(json!({
            "name": "Metro16-DANTE2AVB",
            "host": "Metro16D-02775b.local",
            "ip": "44.44.44.184",
            "port": 4440,
            "manufacturer": "PreSonus",
            "engine": "Audinate DCM",
            "version": "4.0.1",
        }));
    }
    if !has(&ravenna_streams, "32ME-STREAM-1") {
        for n in ["32ME-STREAM-1", "32ME-STREAM-2"] {
            ravenna_streams.push(json!({
                "name": n,
                "host": "WBS-32me.local",
                "ip": "44.44.44.158",
                "port": 554,
                "url": format!("rtsp://44.44.44.158:554/{}", n),
                "standard": "RAVENNA / AES67 (8ch L24 @ 48kHz)",
            }));
        }
    }
    if !has(&nmos_nodes, "WBS-32me") {
        nmos_nodes.push(json!({
            "name": "WBS-32me (uNet2500 Archwave)",
            "host": "WBS-32me.local",
            "ip": "44.44.44.158",
            "port": 80,
            "api": "http://44.44.44.158:80/x-nmos/v1.0/node",
        }));
    }

    // 4. Standard AES67 SAP multicast entry
    let sap_announcements = vec![json!({
        "session_name": "Ward-Beck-AES67-Master",
        "multicast_ip": "239.69.1.1",
        "port": 5004,
        "format": "L24 @ 48kHz Stereo",
        "sdp_sap": "239.255.255.255:9875",
        "clock_sync": "PTP IEEE 1588-2008 (SMPTE 2059-2)",
    })];

    let channel_count = dante_channels.len();
    dante_channels.truncate(16);
    let result = json!({
        "status": "online",
        "dante": {
            "device_count": dante_devices.len(),
            "devices": dante_devices,
            "channel_count": channel_count,
            "channels": dante_channels,
        },
        "ravenna": {
            "stream_count": ravenna_streams.len(),
            "streams": ravenna_streams,
            "module": "Archwave uNet2500 / AMS8E-1",
        },
        "nmos": {
            "node_count": nmos_nodes.len(),
            "nodes": nmos_nodes,
        },
        "aes67_sap": {
            "active_announcements": sap_announcements,
            "sap_port": 9875,
            "sap_group": "239.255.255.255",
        },
        "likedotaudio_ecosystem": {
            "moip_buddy": "https://github.com/LikeDotAudio/APK.softapp.EDITORS.MoipBuddy",
            "dante_docker_plugin": "https://github.com/LikeDotAudio/APK.audio.Docker.PLUGIN.audio.DANTE",
            "supported_protocols": ["Dante", "Ravenna", "AES67", "SMPTE-ST-2110-30", "SAP", "SIP", "NMOS-IS04/05"],
        },
    });
    *AOIP_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some((result.clone(), now_f64()));
    result
}

// ---------------------------------------------------------------------------
// Full report
// ---------------------------------------------------------------------------

/// Layer-3 telemetry when no engine is running: list domain folders under the MXL root.
fn mxl_fallback() -> Value {
    let shm_path = shm_base();
    let exists = std::path::Path::new(&shm_path).exists();
    let mut domains: Vec<String> = Vec::new();
    if exists {
        if let Ok(rd) = std::fs::read_dir(&shm_path) {
            for e in rd.flatten() {
                if e.path().is_dir() {
                    domains.push(e.file_name().to_string_lossy().into_owned());
                }
            }
        }
    }
    let mut m = Map::new();
    m.insert("domain_id".into(), json!(domains.first().cloned().unwrap_or_else(|| "none".into())));
    m.insert("shm_base_dir".into(), json!(shm_path));
    m.insert("flows".into(), json!([]));
    m.insert("available".into(), json!(exists));
    Value::Object(m)
}

/// The four layers. `mxl_telemetry` is the MXL engine's (None: read the folders under the MXL root).
pub fn inspect_full_pipeline(mxl_telemetry: Option<Value>) -> Value {
    let mxl_info = match mxl_telemetry {
        Some(t) if !t.is_null() => t,
        _ => mxl_fallback(),
    };
    json!({
        "timestamp": now_f64(),
        "architecture": "Layered Linux Audio Pipeline & DMF/MXL Media Exchange",
        "layers": {
            "layer1_kernel_hardware": {
                "name": "Kernel & Hardware Layer (ALSA)",
                "description": "Kernel drivers, hardware mixer controls, and low-level PCM audio interfaces",
                "telemetry": inspect_alsa(),
            },
            "layer2_graph_session": {
                "name": "Graph & Session Layer (PipeWire & WirePlumber)",
                "description": "Unified multimedia graph server, routing nodes, links, and audio quantum management",
                "telemetry": inspect_pipewire(),
            },
            "layer3_shared_memory_mxl": {
                "name": "High-Throughput Shared-Memory Layer (EBU DMF / MXL)",
                "description": "Lock-free circular ring buffers in POSIX shared memory (/dev/shm/mxl) for Float32 continuous audio",
                "telemetry": mxl_info,
            },
            "layer4_network_aoip": {
                "name": "Network Audio-over-IP (AoIP) Layer",
                "description": "Dante, Ravenna/AES67 (Archwave), SAP announcements, NMOS IS-04 & MoipBuddy integration",
                "telemetry": inspect_aoip(true),
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_matches_python() {
        assert_eq!(py_round(8192.0 / 48000.0 * 1000.0, 2), 170.67);
        assert_eq!(py_round(256.0 / 48000.0 * 1000.0, 2), 5.33);
        assert_eq!(py_round(2.675, 2), 2.67); // Python: round(2.675, 2) == 2.67
        assert_eq!(py_round(0.125, 2), 0.12); // exact tie -> even
    }

    #[test]
    fn amixer_brackets() {
        let sc = "Simple mixer control 'Master',0\n  Front Left: Playback 55705 [85%] [-4.20dB] [on]\n";
        assert_eq!(first_bracket(sc, is_percent).as_deref(), Some("85%"));
        assert_eq!(first_bracket(sc, is_db).as_deref(), Some("-4.20dB"));
        assert_eq!(first_bracket(sc, |s| s == "on" || s == "off").as_deref(), Some("on"));
        assert_eq!(first_quoted("Simple mixer control 'Master',0").as_deref(), Some("Master"));
        assert_eq!(first_quoted("x '' 'A'").as_deref(), Some(" "));
    }

    #[test]
    fn wpctl_sections() {
        let out = "Audio\n ├─ Devices:\n │      33. Dev [alsa]\n │  \n ├─ Sinks:\n │  *   84. Sink\n └─ Streams:\n        55. parec   \n            150. input_FR        < X:monitor_FR\t[active]\n\nVideo\n ├─ Devices:\nSettings\n";
        let [d, s, src, st] = parse_wpctl(out);
        assert!(d.is_empty() && s.is_empty() && src.is_empty());
        assert_eq!(st, vec!["55. parec".to_string(), "150. input_FR        < X:monitor_FR\t[active]".to_string()]);
    }

    #[test]
    fn avahi_txt() {
        let out = "+;eth0;IPv4;x;_rtsp._tcp;local\n=;eth0;IPv4;Ch1\\064Dev;_netaudio-chan._udp;local;dev.local;10.0.0.1;4455;\"rate=96000\" \"enc=32\"\n";
        let recs = parse_avahi(out);
        assert_eq!(recs.len(), 1);
        let txt = recs[0].0[9..].join(";");
        assert_eq!(txt_int(&txt, "rate="), Some(96000));
        assert_eq!(txt_int(&txt, "enc="), Some(32));
        assert_eq!(txt_value("\"mf=\" \"mf=PreSonus\"", "mf=").as_deref(), Some("PreSonus"));
    }

    #[test]
    fn splitlines_like_python() {
        assert_eq!(py_splitlines("a\r\nb\rc\n"), vec!["a", "b", "c"]);
        assert_eq!(py_splitlines(""), Vec::<&str>::new());
    }

    /// Prints live JSON for diffing against the Python daemon: cargo test --lib -- --ignored --nocapture
    #[test]
    #[ignore]
    fn print_live() {
        println!("ALSA {}", inspect_alsa());
        println!("PIPEWIRE {}", inspect_pipewire());
        println!("AOIP {}", inspect_aoip(true));
        println!("FULL {}", inspect_full_pipeline(None));
    }
}
