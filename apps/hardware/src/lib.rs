// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! Hardware outputs: every sound card output (PipeWire sink) the host has, what each one is
//! listening to, and a crosspoint per (source, output) that is a real PipeWire loopback.
//!
//!   GET  /api/hw  -> {"outputs": [...], "sources": [...], "links": [...]}
//!   POST /api/hw  {"source": "<source name>", "sink": "<sink name>", "on": true|false}
//!
//! Sources are the TAP buses (heard through their monitors) and the hardware inputs.
//! Links made here carry `wbs_hwout` in their arguments, so the TAP's teardown (which unloads
//! every module whose arguments mention `wbs_`) removes them, while `apply_routes` (which only
//! rebuilds the TAP's own managed loopbacks) leaves them alone. Links the TAP made itself (the
//! speaker leg) are listed too and can be switched off from here.

// Plugged into the host: routes, services and start-up (plugin.rs).
pub mod plugin;
pub use plugin::plug;

use serde_json::{json, Value};

use wbs_common::pactl::{run_cmd, run_mut};

const MARK: &str = "sink_input_properties=media.name=wbs_hwout";

fn pactl_json(what: &str) -> Vec<Value> {
    let (code, out, _) = run_cmd(&["pactl", "-f", "json", "list", what]);
    if code != 0 {
        return Vec::new();
    }
    serde_json::from_str::<Value>(&out).ok().and_then(|v| v.as_array().cloned()).unwrap_or_default()
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// ALSA card number and name ("0", "HDA NVidia"), as /proc/asound/cards lists them.
fn card(v: &Value) -> (String, String) {
    let p = v.get("properties");
    let g = |k: &str| p.and_then(|p| p.get(k)).and_then(|x| x.as_str()).unwrap_or("").to_string();
    (g("alsa.card"), g("alsa.card_name"))
}

/// (module id, source, sink, made here) for every module-loopback.
pub fn loopbacks(mods: &str) -> Vec<(String, String, String, bool)> {
    mods.lines()
        .filter_map(|l| {
            let p: Vec<&str> = l.split('\t').collect();
            if p.len() < 3 || p[1] != "module-loopback" {
                return None;
            }
            let arg = |k: &str| p[2].split_whitespace().find_map(|a| a.strip_prefix(k)).unwrap_or("").to_string();
            Some((p[0].to_string(), arg("source="), arg("sink="), p[2].contains("wbs_hwout")))
        })
        .collect()
}

pub fn status() -> Value {
    let default_sink = run_cmd(&["pactl", "get-default-sink"]).1.trim().to_string();
    let outputs: Vec<Value> = pactl_json("sinks")
        .iter()
        .filter(|x| !s(x, "name").starts_with("wbs_"))
        .map(|x| {
            let name = s(x, "name");
            let (card, card_name) = card(x);
            json!({
                "name": name,
                "label": s(x, "description"),
                "card": card,
                "card_name": card_name,
                "state": s(x, "state"),
                "virtual": name.contains("snd_aloop") || name.contains("null") || name.starts_with("XTM_"),
                "default": name == default_sink,
            })
        })
        .collect();
    // Rows: the TAP's two outputs to hardware ("Ward-Beck TAP 5: HW 1 (Speakers)" -> tag "HW 1",
    // label "Speakers"), then the hardware inputs.
    let mut sources: Vec<Value> = wbs_plugin::buses::WBS_SINKS
        .iter()
        .filter(|(bus, ..)| *bus == wbs_plugin::buses::HW1 || *bus == wbs_plugin::buses::HW2)
        .map(|(bus, desc, _)| {
            let after = desc.split_once(": ").map_or(*desc, |x| x.1);
            let (tag, label) = after.split_once(" (").unwrap_or((after, ""));
            json!({"name": format!("{bus}.monitor"), "tag": tag, "label": label.trim_end_matches(')'), "kind": "out"})
        })
        .collect();
    for x in pactl_json("sources") {
        let name = s(&x, "name");
        if name.ends_with(".monitor") || name.starts_with("wbs_") || name.starts_with("XTM_") {
            continue;
        }
        let (card, card_name) = card(&x);
        sources.push(json!({"name": name, "label": s(&x, "description"), "card": card, "card_name": card_name, "kind": "input"}));
    }
    let mods = run_cmd(&["pactl", "list", "short", "modules"]).1;
    let links: Vec<Value> = loopbacks(&mods)
        .into_iter()
        .map(|(id, src, sink, ours)| json!({"module": id, "source": src, "sink": sink, "tap": !ours}))
        .collect();
    json!({"outputs": outputs, "sources": sources, "links": links, "default_sink": default_sink})
}

/// Make or break the crosspoint (source -> sink). Returns the new status.
pub fn set(source: &str, sink: &str, on: bool) -> Result<Value, String> {
    let ok = |n: &str| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || "._-:".contains(c));
    if !ok(source) || !ok(sink) {
        return Err("bad source or sink name".into());
    }
    let mods = run_cmd(&["pactl", "list", "short", "modules"]).1;
    let existing: Vec<String> = loopbacks(&mods)
        .into_iter()
        .filter(|(_, src, snk, _)| src == source && snk == sink)
        .map(|(id, ..)| id)
        .collect();
    if on && existing.is_empty() {
        let (code, _, err) = run_mut(&[
            "pactl", "load-module", "module-loopback",
            &format!("source={source}"), &format!("sink={sink}"),
            "latency_msec=40", "source_dont_move=true", "sink_dont_move=true", MARK,
        ]);
        if code != 0 && code != -1 {
            return Err(format!("load-module failed: {}", err.trim()));
        }
    } else if !on {
        for id in existing {
            run_mut(&["pactl", "unload-module", &id]);
        }
    }
    Ok(status())
}

#[cfg(test)]
mod tests {
    use super::loopbacks;

    #[test]
    fn parses_loopbacks() {
        let m = "7\tmodule-loopback\tsource=wbs_insert_bus.monitor sink=alsa_output.x latency_msec=10\t\n\
                 9\tmodule-null-sink\tsink_name=wbs_input_bus\t\n\
                 12\tmodule-loopback\tsource=alsa_input.mic sink=alsa_output.y latency_msec=20 sink_input_properties=media.name=wbs_hwout\t";
        let l = loopbacks(m);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0], ("7".into(), "wbs_insert_bus.monitor".into(), "alsa_output.x".into(), false));
        assert_eq!(l[1], ("12".into(), "alsa_input.mic".into(), "alsa_output.y".into(), true));
    }
}
