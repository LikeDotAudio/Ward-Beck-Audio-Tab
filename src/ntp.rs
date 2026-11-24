// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! NTP status and configuration for the Timing tab (GET /api/ntp).
//!
//! Reads whichever daemon disciplines the system clock: chrony (`chronyc -c tracking`)
//! or systemd-timesyncd (`timedatectl`). Read-only: nothing here changes the clock.

use serde_json::{json, Map, Value};
use std::time::Duration;

use crate::util::run_cmd;

const T: Duration = Duration::from_secs(2);

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
    m.insert("offset".into(), json!(us(f[4])));
    m.insert("jitter".into(), json!(us(f[6])));
    m.insert("frequency".into(), json!(format!("{}ppm", f[7])));
    m.insert("root_distance".into(), json!(secs(f[10]).zip(secs(f[11])).map(|(d, e)| format!("{:.3}ms", (d / 2.0 + e) * 1e3))));
    m.insert("poll_interval".into(), json!(format!("{}s", f[12])));
    m.insert("leap".into(), json!(f[13].to_lowercase()));
    m
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
        return json!({
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
    json!({
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
    fn chrony_tracking_csv_parses() {
        let t = parse_chrony_tracking("A9FEA9FE,169.254.169.254,3,1700000000.1,-0.000012,0.000001,0.000020,-2.5,0.0,0.01,0.001,0.0005,64.0,Normal");
        assert_eq!(t["server"], "169.254.169.254");
        assert_eq!(t["offset"], "-12us");
        assert_eq!(t["leap"], "normal");
    }
}
