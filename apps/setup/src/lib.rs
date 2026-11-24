// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! Port of setup_page.py: the /setup deployment guide page.

// Plugged into the host: routes, services and start-up (plugin.rs).
pub mod plugin;
pub use plugin::plug;

pub const SETUP_HTML: &str = include_str!("../web/setup.html");
