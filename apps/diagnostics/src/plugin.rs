// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! Diagnostics plugs in the report and the bundle a tester sends, and the TAP's log tail.

use wbs_plugin::reply::qs_first;
use wbs_plugin::{json_reply, Plugin, Reply, RouteResult};

struct Diagnostics;

impl Plugin for Diagnostics {
    fn name(&self) -> &'static str {
        "diagnostics"
    }
    fn get(&self, path: &str, query: &str) -> Option<RouteResult> {
        Some(Ok(match path {
            "/api/diagnostics" => json_reply(&crate::report()),
            "/api/diagnostics/bundle" => Reply::Raw(
                200,
                vec![
                    ("Content-Type".into(), "text/plain; charset=utf-8".into()),
                    ("Content-Disposition".into(), format!("attachment; filename=\"ward-beck-tap-{}.txt\"", std::env::consts::OS)),
                ],
                crate::bundle().into_bytes(),
            ),
            "/api/log" => {
                let since = qs_first(query, "since").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                let (seq, lines) = wbs_log::tail_since(since, 1000);
                json_reply(&serde_json::json!({"seq": seq, "lines": lines, "file": wbs_log::log_path().display().to_string()}))
            }
            _ => return None,
        }))
    }
}

/// Plug diagnostics into the host.
pub fn plug() {
    wbs_plugin::register(&Diagnostics);
}
