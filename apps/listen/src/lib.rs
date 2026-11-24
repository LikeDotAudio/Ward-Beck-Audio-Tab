// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! Port of listen_picker.py: "what should the meters hear?" (apps making noise / sound-card inputs).
//!
//! 1. Windows making noise: every app with an uncorked playback stream, named by its
//!    desktop window title (X11 _NET_WM_PID walk-up) when one can be found. Picking one
//!    meters that app alone; the others go straight to the speakers. "Everything" meters
//!    all desktop audio (the default).
//! 2. Sound card inputs: physical capture sources (alsa_input.*, bluez_input.*). Picking
//!    one meters that input; desktop audio goes straight to the speakers.
//!
//! Talks to the patchbay only through its service (wbs_plugin::services::patchbay()).

// Plugged into the host: routes, services and start-up (plugin.rs).
pub mod plugin;
pub use plugin::plug;

use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use wbs_plugin::services::{self, Patchbay};
use wbs_pyshim::text::{py_splitlines, py_strip};

fn tap() -> &'static dyn Patchbay {
    services::patchbay()
}
use wbs_common::run_cmd_full;

/// Streams the TAP itself (or its helpers) owns: never listed, never moved.
const OWN_BINARIES: &[&str] = &["pacat", "parec", "pw-cat", "pw-play", "pw-record", "pw-loopback", "paplay"];
const MEDIA_HINTS: &[&str] = &[
    "youtube", "spotify", "music", "netflix", "twitch", "soundcloud",
    "bandcamp", "vimeo", "podcast", "radio", "vlc", "player",
];
const METER_BUS: &str = "wbs_input_bus";

/// MODE: app = Null (everything) or the picked app key (kept as the raw JSON value the
/// client sent, like the Python); input = a sound card input source name.
struct Mode {
    app: Value,
    input: Option<String>,
}

static MODE: Mutex<Mode> = Mutex::new(Mode { app: Value::Null, input: None });

fn mode() -> std::sync::MutexGuard<'static, Mode> {
    MODE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Python _run: stdout if the command exits 0, else "".
fn run(cmd: &[&str], timeout: f64) -> String {
    match run_cmd_full(cmd, Duration::from_secs_f64(timeout)) {
        Some((out, 0)) => out,
        _ => String::new(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Stream {
    pub id: String,
    pub sink: String,
    pub corked: bool,
    pub props: HashMap<String, String>,
}

impl Stream {
    fn prop(&self, k: &str) -> &str {
        self.props.get(k).map(String::as_str).unwrap_or("")
    }

    /// application.name or application.process.binary or "Unknown"
    fn key(&self) -> String {
        [self.prop("application.name"), self.prop("application.process.binary")]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or("Unknown")
            .to_string()
    }
}

/// Parse `pactl list sink-inputs` (all streams, before filtering).
pub(crate) fn parse_sink_inputs(out: &str) -> Vec<Stream> {
    let mut streams: Vec<Stream> = Vec::new();
    for line in py_splitlines(out) {
        let s = py_strip(line);
        if let Some(rest) = s.strip_prefix("Sink Input #") {
            let id: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !id.is_empty() {
                streams.push(Stream { id, sink: String::new(), corked: false, props: HashMap::new() });
                continue;
            }
        }
        let Some(cur) = streams.last_mut() else { continue };
        if let Some(v) = s.strip_prefix("Sink: ") {
            cur.sink = py_strip(v).to_string();
        } else if let Some(v) = s.strip_prefix("Corked: ") {
            cur.corked = py_strip(v) == "yes";
        } else if let Some((k, v)) = s.split_once(" = ") {
            cur.props.insert(py_strip(k).to_string(), py_strip(v).trim_matches('"').to_string());
        }
    }
    streams
}

/// Drop streams the TAP owns (helper binaries, loopbacks, this process).
pub(crate) fn filter_own(streams: Vec<Stream>, own_pid: u32) -> Vec<Stream> {
    let own = own_pid.to_string();
    streams
        .into_iter()
        .filter(|st| {
            let binary = st.prop("application.process.binary");
            let name = format!("{} {}", st.prop("application.name"), st.prop("media.name")).to_lowercase();
            !(OWN_BINARIES.contains(&binary)
                || name.contains("loopback")
                || st.props.get("application.process.id").map(|p| *p == own).unwrap_or(false))
        })
        .collect()
}

pub(crate) fn list_streams() -> Vec<Stream> {
    filter_own(parse_sink_inputs(&run(&["pactl", "list", "sink-inputs"], 2.0)), std::process::id())
}

/// re.findall(r"0x[0-9a-f]+", s)
pub(crate) fn find_hex_ids(s: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b'0' && b[i + 1] == b'x' {
            let mut j = i + 2;
            while j < b.len() && matches!(b[j], b'0'..=b'9' | b'a'..=b'f') {
                j += 1;
            }
            if j > i + 2 {
                out.push(s[i..j].to_string());
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// re.search(r"= (\d+)", s) -> int
pub(crate) fn parse_wm_pid(s: &str) -> Option<i64> {
    let mut pos = 0;
    while let Some(i) = s[pos..].find("= ") {
        let start = pos + i + 2;
        let d: String = s[start..].chars().take_while(|c| c.is_ascii_digit()).collect();
        if !d.is_empty() {
            return d.parse().ok();
        }
        pos = pos + i + 1;
    }
    None
}

/// re.search(r'= "(.*)"', s): first `= "` with a later quote on the same line, greedy.
pub(crate) fn parse_wm_name(s: &str) -> Option<String> {
    let mut pos = 0;
    while let Some(i) = s[pos..].find("= \"") {
        let start = pos + i + 3;
        let rest = &s[start..];
        let line = &rest[..rest.find('\n').unwrap_or(rest.len())];
        if let Some(q) = line.rfind('"') {
            return Some(line[..q].to_string());
        }
        pos = pos + i + 1;
    }
    None
}

/// [(pid, title)] for every top-level X11 window, [] when not on X11.
fn windows() -> Vec<(i64, String)> {
    if std::env::var("DISPLAY").map(|d| d.is_empty()).unwrap_or(true) {
        return Vec::new();
    }
    let root = run(&["xprop", "-root", "_NET_CLIENT_LIST"], 2.0);
    let mut wins = Vec::new();
    for wid in find_hex_ids(&root) {
        let pid = parse_wm_pid(&run(&["xprop", "-id", &wid, "_NET_WM_PID"], 0.5));
        let name = parse_wm_name(&run(&["xprop", "-id", &wid, "_NET_WM_NAME"], 0.5));
        if let (Some(p), Some(n)) = (pid, name) {
            wins.push((p, n));
        }
    }
    wins
}

/// Parent pid from /proc/<pid>/stat content.
pub(crate) fn stat_ppid(stat: &str) -> Option<i64> {
    let (_, after) = stat.rsplit_once(')')?;
    after.split_whitespace().nth(1)?.parse().ok()
}

fn ancestors(mut pid: i64) -> Vec<i64> {
    let mut chain = Vec::new();
    for _ in 0..12 {
        if pid <= 1 {
            break;
        }
        chain.push(pid);
        match std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok().as_deref().and_then(stat_ppid) {
            Some(p) => pid = p,
            None => break,
        }
    }
    chain
}

pub(crate) fn pick_title(chain: &[i64], wins: &[(i64, String)]) -> String {
    for &anc in chain {
        let titles: Vec<&String> = wins.iter().filter(|(p, _)| *p == anc).map(|(_, t)| t).collect();
        if !titles.is_empty() {
            let media = titles.iter().find(|t| {
                let l = t.to_lowercase();
                MEDIA_HINTS.iter().any(|h| l.contains(h))
            });
            return media.unwrap_or(&titles[0]).to_string();
        }
    }
    String::new()
}

/// Python int(s) for a decimal string (surrounding whitespace, sign, underscores).
fn py_int(s: &str) -> Option<i64> {
    let t = py_strip(s);
    let digits = t.strip_prefix(['+', '-']).unwrap_or(t);
    if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') || digits.contains("__") {
        return None;
    }
    t.replace('_', "").parse().ok()
}

struct App {
    key: String,
    title: String,
    playing: bool,
    streams: Vec<String>,
}

/// Group streams into apps (insertion order), titles via `title_of(pid)`.
fn group_apps(streams: &[Stream], mut title_of: impl FnMut(i64) -> String) -> Vec<Value> {
    let mut apps: Vec<App> = Vec::new();
    for st in streams {
        let key = st.key();
        let idx = match apps.iter().position(|a| a.key == key) {
            Some(i) => i,
            None => {
                apps.push(App { key: key.clone(), title: String::new(), playing: false, streams: Vec::new() });
                apps.len() - 1
            }
        };
        let app = &mut apps[idx];
        app.streams.push(st.id.clone());
        if !st.corked {
            app.playing = true;
        }
        if app.title.is_empty() {
            let pid = st.props.get("application.process.id").map(String::as_str).unwrap_or("0");
            if let Some(pid) = py_int(pid) {
                app.title = title_of(pid);
            }
        }
    }
    apps.into_iter()
        .filter_map(|mut a| {
            // "Song | YouTube Music - Google Chrome" -> "Song | YouTube Music"
            let suffix = format!(" - {}", a.key);
            if let Some(t) = a.title.strip_suffix(&suffix) {
                a.title = t.to_string();
            }
            a.playing.then(|| {
                json!({"key": a.key, "name": a.key, "title": a.title, "playing": a.playing, "streams": a.streams})
            })
        })
        .collect()
}

/// One entry per app that is making noise (has an uncorked stream).
pub fn apps() -> Vec<Value> {
    let wins = windows();
    group_apps(&list_streams(), |pid| pick_title(&ancestors(pid), &wins))
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Physical sound card capture sources with their live level.
pub fn inputs() -> Vec<Value> {
    let meters = tap().source_meters();
    let mut out = Vec::new();
    for s in tap().available_sources() {
        let n = s.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        let real = n.starts_with("alsa_input.") || n.starts_with("bluez_input.");
        if real && !n.ends_with(".monitor") && !tap().is_virtual_sink(&n) {
            let label = match s.get("description") {
                Some(d) if truthy(d) => d.clone(),
                _ => Value::String(n.clone()),
            };
            let db = meters.get(&n).and_then(|m| m.get("db")).cloned().unwrap_or(json!(-60.0));
            out.push(json!({"name": n, "label": label, "db": db}));
        }
    }
    out
}

/// Sink a stream with app key `key` should be on for the current pick.
fn wanted_sink<'a>(app: &Value, input: &Option<String>, key: &str, speakers: &'a str) -> &'a str {
    if input.as_deref().map(|s| !s.is_empty()).unwrap_or(false) {
        speakers
    } else if app.is_null() || app.as_str() == Some(key) {
        METER_BUS
    } else {
        speakers
    }
}

/// Keep every app stream on the right sink for the current pick (new apps included).
pub fn enforce() {
    let speakers = tap().physical_sink();
    if speakers.is_empty() {
        return;
    }
    let (app, input) = {
        let m = mode();
        (m.app.clone(), m.input.clone())
    };
    let mut sinks: HashMap<String, String> = HashMap::new();
    for line in py_splitlines(&run(&["pactl", "list", "short", "sinks"], 2.0)) {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 2 {
            sinks.insert(parts[0].to_string(), parts[1].to_string());
        }
    }
    for st in list_streams() {
        let want = if wanted_sink(&app, &input, &st.key(), &speakers) == METER_BUS {
            METER_BUS.to_string()
        } else {
            speakers.clone()
        };
        let current = sinks.get(&st.sink).unwrap_or(&st.sink);
        if *current != want {
            run(&["pactl", "move-sink-input", &st.id, &want], 2.0);
        }
    }
}

pub fn state() -> Value {
    let pick = {
        let m = mode();
        let mut p = Map::new();
        p.insert("app".into(), m.app.clone());
        p.insert("input".into(), m.input.clone().map(Value::String).unwrap_or(Value::Null));
        Value::Object(p)
    };
    json!({"apps": apps(), "inputs": inputs(), "pick": pick})
}

/// POST /api/listen body; false if {input} names an unknown source.
/// {app: key|null} meters desktop audio, {input: source} meters a sound card input.
pub fn listen(data: &Value) -> bool {
    match data.get("input") {
        Some(inp) if truthy(inp) => {
            let Some(name) = inp.as_str() else { return false };
            if !tap().available_sources().iter().any(|s| s.get("name").and_then(Value::as_str) == Some(name)) {
                return false;
            }
            {
                let mut m = mode();
                m.app = Value::Null;
                m.input = Some(name.to_string());
            }
            tap().set_selected_source(name);
        }
        _ => {
            {
                let mut m = mode();
                m.app = data.get("app").cloned().unwrap_or(Value::Null);
                m.input = None;
            }
            tap().set_selected_source("wbs_input_bus.monitor");
        }
    }
    tap().apply_routes();
    enforce();
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACTL: &str = "Sink Input #42\n\tDriver: PipeWire\n\tSink: 84\n\tCorked: no\n\tProperties:\n\t\tapplication.name = \"Google Chrome\"\n\t\tapplication.process.id = \"1234\"\n\t\tmedia.name = \"Playback\"\n\nSink Input #43\n\tSink: 84\n\tCorked: yes\n\tProperties:\n\t\tapplication.process.binary = \"parec\"\n\nSink Input #44\n\tSink: 12\n\tCorked: yes\n\tProperties:\n\t\tapplication.name = \"Firefox\"\n\t\tapplication.process.id = \"x\"\n";

    #[test]
    fn parses_and_filters_sink_inputs() {
        let all = parse_sink_inputs(PACTL);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].id, "42");
        assert_eq!(all[0].sink, "84");
        assert!(!all[0].corked && all[1].corked);
        assert_eq!(all[0].prop("application.name"), "Google Chrome");
        let kept = filter_own(all, 1234);
        assert_eq!(kept.len(), 1); // own pid + parec dropped
        assert_eq!(kept[0].id, "44");
    }

    #[test]
    fn groups_apps_and_trims_title_suffix() {
        let all = parse_sink_inputs(PACTL);
        let apps = group_apps(&all, |pid| if pid == 1234 { "Song | YouTube Music - Google Chrome".into() } else { String::new() });
        assert_eq!(apps.len(), 1); // Firefox corked, parec corked
        assert_eq!(apps[0].to_string(), r#"{"key":"Google Chrome","name":"Google Chrome","title":"Song | YouTube Music","playing":true,"streams":["42"]}"#);
    }

    #[test]
    fn xprop_parsing() {
        assert_eq!(find_hex_ids("_NET_CLIENT_LIST(WINDOW): window id # 0x1a00003, 0x2e00004, 0xZ"), vec!["0x1a00003", "0x2e00004"]);
        assert_eq!(parse_wm_pid("_NET_WM_PID(CARDINAL) = 4242\n"), Some(4242));
        assert_eq!(parse_wm_pid("_NET_WM_PID:  not found.\n"), None);
        assert_eq!(parse_wm_name("_NET_WM_NAME(UTF8_STRING) = \"a \\\"b\\\" c\"\n").as_deref(), Some("a \\\"b\\\" c"));
        assert_eq!(stat_ppid("123 (we(ird) name) S 77 123 123"), Some(77));
    }

    #[test]
    fn title_prefers_media_hint() {
        let wins = vec![(5, "Terminal".to_string()), (5, "Spotify Premium".to_string()), (9, "Other".to_string())];
        assert_eq!(pick_title(&[3, 5, 9], &wins), "Spotify Premium");
        assert_eq!(pick_title(&[3], &wins), "");
    }

    #[test]
    fn wanted_sink_rules() {
        let spk = "alsa_output.x";
        assert_eq!(wanted_sink(&Value::Null, &None, "A", spk), METER_BUS);
        assert_eq!(wanted_sink(&json!("A"), &None, "A", spk), METER_BUS);
        assert_eq!(wanted_sink(&json!("B"), &None, "A", spk), spk);
        assert_eq!(wanted_sink(&Value::Null, &Some("alsa_input.y".into()), "A", spk), spk);
    }

    #[test]
    fn py_int_like_python() {
        assert_eq!(py_int(" 12 "), Some(12));
        assert_eq!(py_int("1_0"), Some(10));
        assert_eq!(py_int("x"), None);
        assert_eq!(py_int(""), None);
    }
}

#[cfg(test)]
mod live {
    /// Prints the apps list for diffing against GET /api/listen: cargo test --lib -- --ignored --nocapture
    #[test]
    #[ignore]
    fn print_live_apps() {
        println!("APPS {}", serde_json::Value::Array(super::apps()));
    }
}
