// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.com - Ward-Beck.ca). All rights reserved.
//! The plugin contract of the Ward-Beck Audio Tap.
//!
//! Every application (apps/<name>) is a crate that depends only on this contract and on the
//! utilities (utils/<name>), never on another application. It plugs into the host in two ways:
//!
//! - **Plugin**: its HTTP routes and its background start-up, registered with the host
//!   (`register`). The host offers each request to every plugin in turn.
//! - **Services**: what it offers the other applications (`services::provide_*`), and what it
//!   asks of them (`services::mxl()`, `services::aoip()`, ...). A service nobody provides
//!   answers with an inert default, so any application can be left out of a build.

pub mod buses;
pub mod reply;
pub mod services;

pub use reply::{json_reply, Reply, RouteResult};

use std::sync::{OnceLock, RwLock};

/// One application plugged into the host.
pub trait Plugin: Send + Sync {
    /// Short name, for the log and the diagnostics report.
    fn name(&self) -> &'static str;
    /// Background work, once every plugin is registered and every service provided.
    fn start(&self) {}
    /// GET `path` (`query` is the raw query string): Some(answer) if this plugin owns the route.
    fn get(&self, _path: &str, _query: &str) -> Option<RouteResult> {
        None
    }
    /// POST `path` with its body: Some(answer) if this plugin owns the route.
    fn post(&self, _path: &str, _body: &str) -> Option<RouteResult> {
        None
    }
}

fn registry() -> &'static RwLock<Vec<&'static dyn Plugin>> {
    static R: OnceLock<RwLock<Vec<&'static dyn Plugin>>> = OnceLock::new();
    R.get_or_init(|| RwLock::new(Vec::new()))
}

/// Plug an application in (the host does this for every application it is built with).
pub fn register(p: &'static dyn Plugin) {
    registry().write().unwrap_or_else(|e| e.into_inner()).push(p);
}

/// Every plugged-in application, in registration order.
pub fn plugins() -> Vec<&'static dyn Plugin> {
    registry().read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Start every plugin's background work.
pub fn start_all() {
    for p in plugins() {
        p.start();
        wbs_log::info("plugin", &format!("{} ready", p.name()));
    }
}

/// The first plugin that owns GET `path`.
pub fn route_get(path: &str, query: &str) -> Option<RouteResult> {
    plugins().into_iter().find_map(|p| p.get(path, query))
}

/// The first plugin that owns POST `path`.
pub fn route_post(path: &str, body: &str) -> Option<RouteResult> {
    plugins().into_iter().find_map(|p| p.post(path, body))
}
