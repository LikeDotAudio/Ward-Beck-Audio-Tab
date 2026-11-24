// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! What a tester sends back: GET /api/diagnostics (JSON) and GET /api/diagnostics/bundle
//! (one text file: the report, every recent problem and the log tail). Written for a
//! machine nobody here can see, so it states everything it can find out by itself.

// Plugged into the host: routes, services and start-up (plugin.rs).
pub mod plugin;
pub use plugin::plug;

use serde_json::{json, Value};
use std::time::Instant;

use wbs_common as util;
use wbs_plugin::services;

fn started() -> &'static Instant {
    static T: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    T.get_or_init(Instant::now)
}

/// Call once at startup so uptime counts from there.
pub fn mark_start() {
    let _ = started();
}

/// The audio back end this build uses on this OS.
pub fn audio_backend() -> &'static str {
    if cfg!(target_os = "linux") {
        "PipeWire / PulseAudio (pactl, parec, pacat)"
    } else if cfg!(windows) {
        "WASAPI loopback (driverless)"
    } else if cfg!(target_os = "macos") {
        "Core Audio process tap (driverless, macOS 14.2+)"
    } else {
        "none"
    }
}

/// The programs the Tap calls on this OS, and whether each is on PATH.
pub fn tools() -> Value {
    let names: &[&str] = if cfg!(target_os = "linux") {
        &["pactl", "parec", "pacat", "pw-cli", "pw-dump", "pw-top", "ip", "timedatectl", "chronyc", "docker", "setcap", "getcap"]
    } else if cfg!(windows) {
        &["w32tm", "docker", "powershell"]
    } else {
        &["sntp", "systemsetup", "docker"]
    };
    Value::Object(names.iter().map(|n| (n.to_string(), json!(util::which(n)))).collect())
}

/// Linux: does the installed binary hold the PTP capabilities?
fn capabilities() -> Value {
    if !cfg!(target_os = "linux") {
        return Value::Null;
    }
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
    json!(util::run_cmd(&["getcap", &exe], std::time::Duration::from_secs(2)).unwrap_or_default().trim())
}

pub fn report() -> Value {
    let aoip = services::aoip().snapshot();
    let ptp = &aoip["ptp"];
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| util::run_cmd(&["hostname"], std::time::Duration::from_secs(2)).unwrap_or_default().trim().to_string());
    json!({
        "tap": {
            "version": env!("CARGO_PKG_VERSION"),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "host": host,
            "uptime_s": started().elapsed().as_secs(),
            "port": util::port(),
            "exe": std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default(),
            "log_file": wbs_log::log_path().display().to_string(),
            "audio_backend": audio_backend(),
            "shm_base": util::shm_base(),
            "features": services::aoip().features(),
        },
        "tools": tools(),
        "capabilities": capabilities(),
        "clients": wbs_log::clients_json(),
        "ptp": {
            "mode": ptp["mode"], "listening": ptp["listening"], "error": ptp["error"],
            "clocks": ptp["clocks"].as_array().map(|a| a.len()).unwrap_or(0),
            "grandmasters": ptp["grandmasters"],
            "leader": services::aoip().ptp_leader().map(|(gm, d)| json!({"identity": gm, "domain": d})),
        },
        "sap": {"listening": aoip["sap"]["listening"], "error": aoip["sap"]["error"], "sessions": aoip["sap"]["sessions"].as_array().map(|a| a.len()).unwrap_or(0)},
        "ntp": services::ntp().status(),
        "aes67": services::aes67().status()["transmitters"].as_array().map(|t| t.iter().map(|x| json!({"flow": x["flow_id"], "running": x["running"], "error": x["error"], "packets": x["packets"]})).collect::<Vec<_>>()),
        "recent_problems": wbs_log::recent_problems(50),
    })
}

/// One text file for a bug report: the report, then the last 1500 log lines.
pub fn bundle() -> String {
    let (_, lines) = wbs_log::tail_since(0, 1500);
    format!(
        "Ward-Beck Audio Tap diagnostic bundle\nmade {}\n\n== report ==\n{}\n\n== log (last {} lines; full file: {}) ==\n{}\n",
        wbs_log::timestamp(),
        serde_json::to_string_pretty(&report()).unwrap_or_default(),
        lines.len(),
        wbs_log::log_path().display(),
        lines.join("\n")
    )
}

/// The first lines of every log: what is running where, and what is missing.
pub fn startup_banner() {
    mark_start();
    let log = |m: &str| wbs_log::info("start", m);
    log(&format!(
        "Ward-Beck Audio Tap {} on {} {} · port {} · audio: {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        util::port(),
        audio_backend()
    ));
    log(&format!("log file: {}", wbs_log::log_path().display()));
    log(&format!("MXL shared memory: {}", util::shm_base()));
    if let Value::Object(t) = tools() {
        let missing: Vec<&String> = t.iter().filter(|(_, v)| *v == &json!(false)).map(|(k, _)| k).collect();
        let found: Vec<&String> = t.iter().filter(|(_, v)| *v == &json!(true)).map(|(k, _)| k).collect();
        log(&format!("tools found: {}", if found.is_empty() { "none".into() } else { found.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ") }));
        // The audio tools are required; the rest are used when present (chrony, docker, ...).
        let required = ["pactl", "parec", "pacat"];
        let (need, opt): (Vec<&&String>, Vec<&&String>) = missing.iter().partition(|m| required.contains(&m.as_str()));
        if !need.is_empty() {
            wbs_log::error("start", &format!("required tools missing: {} (no audio buses, meters or MXL input without them)", need.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ")));
        }
        if !opt.is_empty() {
            log(&format!("optional tools not installed: {}", opt.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ")));
        }
    }
    if let Value::String(c) = capabilities() {
        if c.is_empty() {
            wbs_log::warn("start", "no PTP capabilities on this binary (the Timing tab shows the setcap command)");
        } else {
            log(&format!("capabilities: {c}"));
        }
    }
}
