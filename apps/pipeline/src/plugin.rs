// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.com - Ward-Beck.ca). All rights reserved.
//! The pipeline inspector plugs in /api/pipeline* and offers its reports.

use serde_json::Value;
use wbs_plugin::{json_reply, services, Plugin, RouteResult};

struct Pipeline;

/// The four layers, with the MXL engine's telemetry when an engine is plugged in.
pub fn full() -> Value {
    crate::inspect_full_pipeline(Some(services::mxl().telemetry()))
}

impl Plugin for Pipeline {
    fn name(&self) -> &'static str {
        "pipeline"
    }
    fn get(&self, path: &str, _query: &str) -> Option<RouteResult> {
        Some(Ok(json_reply(&match path {
            "/api/pipeline" => full(),
            "/api/pipeline/alsa" => crate::inspect_alsa(),
            "/api/pipeline/pipewire" => crate::inspect_pipewire(),
            _ => return None,
        })))
    }
}

impl services::Pipeline for Pipeline {
    fn inspect_full(&self) -> Value {
        full()
    }
    fn inspect_aoip(&self, use_cache: bool) -> Value {
        crate::inspect_aoip(use_cache)
    }
}

/// Plug the pipeline inspector into the host.
pub fn plug() {
    wbs_plugin::register(&Pipeline);
    services::provide_pipeline(Box::new(Pipeline));
}
