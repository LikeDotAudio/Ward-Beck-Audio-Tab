// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! Setup plugs in its one page.

use wbs_plugin::{Plugin, Reply, RouteResult};

struct Setup;

impl Plugin for Setup {
    fn name(&self) -> &'static str {
        "setup"
    }
    fn get(&self, path: &str, _query: &str) -> Option<RouteResult> {
        matches!(path, "/setup" | "/setup/").then(|| Ok(Reply::Html(crate::SETUP_HTML)))
    }
}

/// Plug the /setup page into the host.
pub fn plug() {
    wbs_plugin::register(&Setup);
}
