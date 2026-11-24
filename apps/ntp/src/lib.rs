// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! NTP status and configuration for the Timing tab (GET /api/ntp).
//!
//! Reads whichever daemon disciplines the system clock: chrony (`chronyc -c tracking`)
//! or systemd-timesyncd (`timedatectl`). Read-only: nothing here changes the clock.

// Plugged into the host: routes, services and start-up (plugin.rs).
pub mod plugin;
pub use plugin::plug;

use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use wbs_common::run_cmd;

const T: Duration = Duration::from_secs(2);

/// National Research Council Canada: the official Canadian time servers.
pub const NRC_SERVERS: [&str; 2] = ["time.nrc.ca", "time.chu.nrc.ca"];

/// Is this machine taking its time from the NRC?
pub fn uses_nrc(server: &str) -> bool {
    server.to_lowercase().contains("nrc.ca")
}

/// The interface the default route leaves by (where timesyncd's link servers apply).
fn default_iface() -> Option<String> {
    let out = run_cmd(&["ip", "route", "show", "default"], T)?;
    let mut it = out.split_whitespace();
    while let Some(w) = it.next() {
        if w == "dev" {
            return it.next().map(String::from);
        }
    }
    None
}

/// The runtime prompt that points NTP at the NRC (it needs sudo, so the user runs it).
pub fn nrc_fix(daemon: &str, iface: Option<&str>) -> Value {
    let servers = NRC_SERVERS.join(" ");
    let mut options = Vec::new();
    if daemon == "chrony" {
        options.push(json!({"label": "This session only", "note": "until chrony restarts",
            "command": NRC_SERVERS.iter().map(|s| format!("sudo chronyc add server {s} iburst")).collect::<Vec<_>>().join(" && ")}));
        options.push(json!({"label": "Permanently", "note": "saved as /etc/chrony/sources.d/nrc.sources",
            "command": format!("printf 'server %s iburst\\n' {servers} | sudo tee /etc/chrony/sources.d/nrc.sources && sudo chronyc reload sources")}));
    } else {
        if let Some(i) = iface {
            options.push(json!({"label": "This session only", "note": format!("link servers on {i}, gone at the next reboot"),
                "command": format!("sudo timedatectl ntp-servers {i} {servers}")}));
        }
        options.push(json!({"label": "Permanently", "note": "saved as /etc/systemd/timesyncd.conf.d/nrc.conf",
            "command": format!("sudo mkdir -p /etc/systemd/timesyncd.conf.d && printf '[Time]\\nNTP={servers}\\n' | sudo tee /etc/systemd/timesyncd.conf.d/nrc.conf && sudo systemctl restart systemd-timesyncd")}));
    }
    json!({
        "title": "Take time from the NRC",
        "why": format!("Canada's official time comes from the National Research Council ({}). \
                        Changing the NTP servers needs your password, so run one of these in a terminal.", NRC_SERVERS.join(", ")),
        "options": options,
    })
}

/// `Key=Value` lines (timedatectl show / show-timesync) into a map.
pub fn parse_kv(text: &str) -> Map<String, Value> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), json!(v.trim())))
        .collect()
}

/// `Name: value` lines (timedatectl timesync-status) into a map, keys lower_snake.
pub fn parse_colon(text: &str) -> Map<String, Value> {
    text.lines()
        .filter_map(|l| l.split_once(": "))
        .map(|(k, v)| (k.trim().to_lowercase().replace(' ', "_"), json!(v.trim())))
        .collect()
}

/// The lines a config file actually sets: no comments, no blanks.
pub fn active_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with(';'))
        .map(String::from)
        .collect()
}

/// Every config file for the daemon (main file, then drop-ins in name order).
fn config_files(main: &str, dropin_dirs: &[&str]) -> Vec<Value> {
    let mut files = vec![main.to_string()];
    for dir in dropin_dirs {
        if let Ok(rd) = std::fs::read_dir(dir) {
            let mut v: Vec<String> = rd
                .filter_map(|e| e.ok())
                .map(|e| e.path().display().to_string())
                .filter(|p| p.ends_with(".conf") || p.ends_with(".sources"))
                .collect();
            v.sort();
            files.extend(v);
        }
    }
    files
        .into_iter()
        .filter_map(|f| std::fs::read_to_string(&f).ok().map(|t| json!({"file": f, "lines": active_lines(&t)})))
        .collect()
}

/// chronyc -c tracking (CSV) into the status fields.
pub fn parse_chrony_tracking(csv: &str) -> Map<String, Value> {
    let f: Vec<&str> = csv.trim().split(',').collect();
    let mut m = Map::new();
    if f.len() < 14 {
        return m;
    }
    let secs = |s: &str| s.parse::<f64>().ok();
    let us = |s: &str| secs(s).map(|v| format!("{:+.0}us", v * 1e6));
    m.insert("server".into(), json!(f[1]));
    m.insert("stratum".into(), json!(f[2]));
    m.insert("ref_time".into(), json!(f[3])); // changes on every update the server sends
    m.insert("offset".into(), json!(us(f[4])));
    m.insert("jitter".into(), json!(us(f[6])));
    m.insert("frequency".into(), json!(format!("{}ppm", f[7])));
    m.insert("root_distance".into(), json!(secs(f[10]).zip(secs(f[11])).map(|(d, e)| format!("{:.3}ms", (d / 2.0 + e) * 1e3))));
    m.insert("poll_interval".into(), json!(format!("{}s", f[12])));
    m.insert("leap".into(), json!(f[13].to_lowercase()));
    m
}

// ---------------------------------------------------------------------------
// Receipts: every NTP answer the clock took, as the TAP saw it
// ---------------------------------------------------------------------------

const RECEIPTS_MAX: usize = 200;
const RECEIPT_EVERY: Duration = Duration::from_secs(10);
static RECEIPTS: Mutex<VecDeque<Value>> = Mutex::new(VecDeque::new());

/// What changes when a new answer arrives: timesyncd's packet count, chrony's reference time.
pub fn receipt_key(st: &Value) -> Option<String> {
    let s = st.get("status")?;
    let k = s.get("packets").or_else(|| s.get("ref_time"))?;
    let k = match k { Value::String(v) => v.clone(), Value::Null => return None, v => v.to_string() };
    Some(format!("{}|{}", s.get("server").and_then(Value::as_str).unwrap_or(""), k))
}

/// The receipt for a status: when, from whom, and what the clock made of it.
pub fn receipt_of(st: &Value, t: f64, first: bool) -> Value {
    let s = &st["status"];
    let g = |k: &str| s.get(k).cloned().unwrap_or(Value::Null);
    json!({
        "t": t, "first": first, "server": g("server"), "stratum": g("stratum"),
        "offset": g("offset"), "delay": g("delay"), "jitter": g("jitter"),
        "poll": g("poll_interval"), "packets": g("packets"), "synchronized": st["synchronized"],
    })
}

/// Background: watch the NTP daemon and log each answer it takes (the Timing tab's NTP log).
pub fn receipts_loop() {
    let mut last: Option<String> = None;
    loop {
        let st = status();
        if let Some(key) = receipt_key(&st) {
            if last.as_deref() != Some(key.as_str()) {
                let r = receipt_of(&st, wbs_common::now_f64(), last.is_none());
                wbs_log::info(
                    "ntp",
                    &format!(
                        "{} {} offset {} delay {} (stratum {})",
                        if last.is_none() { "last answer" } else { "answer" },
                        r["server"].as_str().unwrap_or("?"),
                        r["offset"].as_str().unwrap_or("?"),
                        r["delay"].as_str().unwrap_or("?"),
                        r["stratum"].as_str().unwrap_or("?")
                    ),
                );
                let mut q = RECEIPTS.lock().unwrap_or_else(|e| e.into_inner());
                q.push_back(r);
                while q.len() > RECEIPTS_MAX {
                    q.pop_front();
                }
                last = Some(key);
            }
        }
        std::thread::sleep(RECEIPT_EVERY);
    }
}

/// GET /api/ntp: the status plus the receipts log, newest first.
pub fn report() -> Value {
    let mut st = status();
    let q = RECEIPTS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(o) = st.as_object_mut() {
        o.insert("receipts".into(), Value::Array(q.iter().rev().cloned().collect()));
    }
    st
}

pub fn status() -> Value {
    let show = parse_kv(&run_cmd(&["timedatectl", "show"], T).unwrap_or_default());
    let enabled = show.get("NTP").and_then(Value::as_str) == Some("yes");
    let synced = show.get("NTPSynchronized").and_then(Value::as_str) == Some("yes");
    let tz = show.get("Timezone").cloned().unwrap_or(Value::Null);

    if let Some(csv) = run_cmd(&["chronyc", "-c", "tracking"], T).filter(|s| s.contains(',')) {
        let t = parse_chrony_tracking(&csv);
        let sources = run_cmd(&["chronyc", "-c", "sources"], T)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split(',').nth(2).map(String::from))
            .collect::<Vec<_>>();
        let server = t.get("server").and_then(Value::as_str).unwrap_or("").to_string();
        let nrc = uses_nrc(&server) || sources.iter().any(|s| uses_nrc(s));
        return json!({
            "nrc": nrc,
            "fix": if nrc { Value::Null } else { nrc_fix("chrony", None) },
            "daemon": "chrony",
            "enabled": enabled,
            "synchronized": synced,
            "timezone": tz,
            "status": t,
            "servers": {"sources": sources},
            "config": config_files("/etc/chrony/chrony.conf", &["/etc/chrony/conf.d", "/etc/chrony/sources.d"]),
        });
    }

    let sync = parse_kv(&run_cmd(&["timedatectl", "show-timesync", "--all"], T).unwrap_or_default());
    let st = parse_colon(&run_cmd(&["timedatectl", "timesync-status"], T).unwrap_or_default());
    // "34min 8s (min: 32s; max 34min 8s)" -> "34min 8s": the limits are shown once, under config.
    let pick = |k: &str| match st.get(k).and_then(Value::as_str) {
        Some(v) => json!(v.split(" (").next().unwrap_or(v)),
        None => Value::Null,
    };
    let list = |k: &str| {
        let v = sync.get(k).and_then(Value::as_str).unwrap_or("");
        json!(v.split_whitespace().collect::<Vec<_>>())
    };
    let server = sync.get("ServerName").and_then(Value::as_str).unwrap_or("").to_string();
    let nrc = uses_nrc(&server);
    let iface = default_iface();
    json!({
        "nrc": nrc,
        "fix": if nrc { Value::Null } else { nrc_fix("systemd-timesyncd", iface.as_deref()) },
        "daemon": if sync.is_empty() { "unknown" } else { "systemd-timesyncd" },
        "enabled": enabled,
        "synchronized": synced,
        "timezone": tz,
        "status": {
            "server": pick("server"),
            "stratum": pick("stratum"),
            "offset": pick("offset"),
            "delay": pick("delay"),
            "jitter": pick("jitter"),
            "poll_interval": pick("poll_interval"),
            "root_distance": pick("root_distance"),
            "leap": pick("leap"),
            "frequency": pick("frequency"),
            "packets": pick("packet_count"),
        },
        "servers": {
            "system": list("SystemNTPServers"),
            "fallback": list("FallbackNTPServers"),
            "link": list("LinkNTPServers"),
            "runtime": list("RuntimeNTPServers"),
        },
        "poll_min": sync.get("PollIntervalMinUSec").cloned().unwrap_or(Value::Null),
        "poll_max": sync.get("PollIntervalMaxUSec").cloned().unwrap_or(Value::Null),
        "root_distance_max": sync.get("RootDistanceMaxUSec").cloned().unwrap_or(Value::Null),
        "config": config_files("/etc/systemd/timesyncd.conf", &["/etc/systemd/timesyncd.conf.d"]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timesyncd_status_and_config_parse() {
        let st = parse_colon("       Server: 185.125.190.58 (ntp.ubuntu.com)\nPoll interval: 34min 8s (min: 32s; max 34min 8s)\n       Offset: +620us\n");
        assert_eq!(st["server"], "185.125.190.58 (ntp.ubuntu.com)");
        assert_eq!(st["poll_interval"], "34min 8s (min: 32s; max 34min 8s)");
        assert_eq!(st["offset"], "+620us");
        let kv = parse_kv("FallbackNTPServers=ntp.ubuntu.com\nSystemNTPServers=\n");
        assert_eq!(kv["FallbackNTPServers"], "ntp.ubuntu.com");
        assert_eq!(active_lines("#  comment\n[Time]\n\n#NTP=\nNTP=10.0.0.1 pool.ntp.org\n"), vec!["[Time]", "NTP=10.0.0.1 pool.ntp.org"]);
    }

    #[test]
    fn nrc_is_offered_until_it_is_used() {
        assert!(uses_nrc("time.chu.nrc.ca") && uses_nrc("132.246.11.227 (time.nrc.ca)") && !uses_nrc("ntp.ubuntu.com"));
        let f = nrc_fix("systemd-timesyncd", Some("enp12s0"));
        assert_eq!(f["options"][0]["command"], "sudo timedatectl ntp-servers enp12s0 time.nrc.ca time.chu.nrc.ca");
        assert!(f["options"][1]["command"].as_str().unwrap().contains("NTP=time.nrc.ca time.chu.nrc.ca"));
        assert!(nrc_fix("chrony", None)["options"][0]["command"].as_str().unwrap().contains("chronyc add server time.nrc.ca"));
    }

    #[test]
    fn a_receipt_is_logged_per_answer() {
        let st = json!({"synchronized": true, "status": {"server": "132.246.11.229 (time.nrc.ca)", "packets": "21", "offset": "-1.292ms", "delay": "28.699ms"}});
        assert_eq!(receipt_key(&st).as_deref(), Some("132.246.11.229 (time.nrc.ca)|21"));
        let r = receipt_of(&st, 5.0, true);
        assert_eq!(r["offset"], "-1.292ms");
        assert_eq!(r["first"], true);
        let chrony = json!({"status": {"server": "1.2.3.4", "ref_time": "1700000000.1"}});
        assert_eq!(receipt_key(&chrony).as_deref(), Some("1.2.3.4|1700000000.1"));
        assert_eq!(receipt_key(&json!({"status": {"server": "x"}})), None);
    }

    #[test]
    fn chrony_tracking_csv_parses() {
        let t = parse_chrony_tracking("A9FEA9FE,169.254.169.254,3,1700000000.1,-0.000012,0.000001,0.000020,-2.5,0.0,0.01,0.001,0.0005,64.0,Normal");
        assert_eq!(t["server"], "169.254.169.254");
        assert_eq!(t["offset"], "-12us");
        assert_eq!(t["leap"], "normal");
    }
}
