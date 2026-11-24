// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! What the applications offer each other. Each application provides its service once at
//! start-up (`provide_*`); every other application reaches it through the getter (`mxl()`,
//! `aoip()`, ...). Every method has an inert default, which is also what a caller gets when
//! the application is not in the build: nothing to show, nothing done, no panic.

use serde_json::{Map, Value};
use std::sync::OnceLock;
use wbs_common::PcmChunk;

macro_rules! slot {
    ($Trait:ident, $get:ident, $provide:ident, $SLOT:ident, $Absent:ident) => {
        static $SLOT: OnceLock<Box<dyn $Trait>> = OnceLock::new();
        struct $Absent;
        impl $Trait for $Absent {}
        /// The provider of this service (an inert default when none is plugged in).
        pub fn $get() -> &'static dyn $Trait {
            $SLOT.get().map(|b| b.as_ref()).unwrap_or(&$Absent)
        }
        /// Offer this service (once, at start-up).
        pub fn $provide(s: Box<dyn $Trait>) {
            let _ = $SLOT.set(s);
        }
    };
}

/// One MXL flow, as other applications see it.
#[derive(Clone, Debug, Default)]
pub struct FlowInfo {
    pub flow_id: String,
    pub label: String,
    pub domain_id: String,
    pub channel_count: usize,
    pub sample_rate: u32,
    pub quantum: usize,
    pub buffer_length: usize,
}

/// MXL / EBU DMF engine (apps/mxl): the shared-memory data plane.
pub trait Mxl: Send + Sync {
    fn start(&self, _source: &str) {}
    fn stop(&self) {}
    fn set_source(&self, _source: &str) {}
    fn start_generator(&self, _name: &str) {}
    fn stop_generators(&self) -> Vec<String> {
        Vec::new()
    }
    fn telemetry(&self) -> Value {
        Value::Null
    }
    fn scan_all_domains(&self) -> Value {
        Value::Object(Map::new())
    }
    fn domain_id(&self) -> String {
        String::new()
    }
    fn external_waveform(&self, _flow_id: &str) -> Value {
        Value::Null
    }
    fn flow_ids(&self) -> Vec<String> {
        Vec::new()
    }
    fn flow_info(&self, _flow_id: &str) -> Option<FlowInfo> {
        None
    }
    fn write_head(&self, _flow_id: &str) -> Option<usize> {
        None
    }
    fn read_planar(&self, _flow_id: &str, _start: usize, _count: usize) -> Vec<Vec<f32>> {
        Vec::new()
    }
    fn flow_pcm(&self, _flow_id: &str, _since: i64, _max_frames: usize) -> Option<PcmChunk> {
        None
    }
    /// (write head, write sequence) of the master flow, for the meter stream.
    fn master_head(&self) -> (usize, u64) {
        (0, 0)
    }
}
slot!(Mxl, mxl, provide_mxl, MXL, NoMxl);

/// Patchbay (apps/patchbay): the TAP buses, routes, sources and their meters.
pub trait Patchbay: Send + Sync {
    fn refresh_sources(&self) {}
    fn apply_routes(&self) {}
    fn available_sources(&self) -> Vec<Value> {
        Vec::new()
    }
    fn is_virtual_sink(&self, _name: &str) -> bool {
        false
    }
    fn physical_sink(&self) -> String {
        String::new()
    }
    fn selected_source(&self) -> String {
        String::new()
    }
    fn set_selected_source(&self, _name: &str) {}
    fn source_meters(&self) -> Value {
        Value::Object(Map::new())
    }
    fn meter_levels(&self) -> Value {
        Value::Object(Map::new())
    }
    /// Tear everything down (shutdown).
    fn abandon_ship(&self) {}
    /// A page is still there (keeps the idle watchdog quiet).
    fn client_checkin(&self) {}
}
slot!(Patchbay, patchbay, provide_patchbay, PATCHBAY, NoPatchbay);

/// AoIP listener (apps/aoip): SAP / SDP, SIP, the PTP sniffer.
pub trait Aoip: Send + Sync {
    fn snapshot(&self) -> Value {
        Value::Object(Map::new())
    }
    /// Make sure the listeners run (a receiver needs SAP).
    fn ensure_started(&self) {}
    /// The PTP leader (grandmaster identity, domain), if one is heard.
    fn ptp_leader(&self) -> Option<(String, i64)> {
        None
    }
    /// Optional features it was built with ({"vendors": true} = PTP clock vendor names).
    fn features(&self) -> Value {
        Value::Object(Map::new())
    }
}
slot!(Aoip, aoip, provide_aoip, AOIP, NoAoip);

/// Receivers (apps/receivers): the test point and the network / MoIP receivers.
pub trait Receivers: Send + Sync {
    fn stop_point(&self) -> bool {
        false
    }
    fn stop_receivers(&self) -> bool {
        false
    }
    /// The test point's ring, then the network / MoIP receivers' rings (as MXL ring reports).
    fn rings(&self, _bins: usize) -> (Option<Value>, Vec<Value>) {
        (None, Vec::new())
    }
    fn test_point_status(&self) -> Value {
        Value::Null
    }
    fn receivers_status(&self) -> Value {
        Value::Null
    }
    /// Audio of a receiver's flow: None when `flow_id` is not a receiver's.
    fn pcm(&self, _flow_id: &str, _since: i64, _max_frames: usize) -> Option<Option<PcmChunk>> {
        None
    }
}
slot!(Receivers, receivers, provide_receivers, RECEIVERS, NoReceivers);

/// AES67 transmitters (apps/aes67).
pub trait Aes67: Send + Sync {
    fn status(&self) -> Value {
        Value::Object(Map::new())
    }
    fn stop_all(&self) -> Vec<String> {
        Vec::new()
    }
}
slot!(Aes67, aes67, provide_aes67, AES67, NoAes67);

/// Hardware outputs (apps/hardware): TAP outputs and hardware inputs to sound card outputs.
pub trait Hardware: Send + Sync {
    fn set(&self, _source: &str, _sink: &str, _on: bool) -> Result<Value, String> {
        Err("no hardware outputs in this build".into())
    }
    /// Is anything linked from `source` to a sound card?
    fn has_link_from(&self, _source: &str) -> bool {
        false
    }
}
slot!(Hardware, hardware, provide_hardware, HARDWARE, NoHardware);

/// NMOS / AoIP device discovery (apps/nmos).
pub trait Nmos: Send + Sync {
    fn nodes(&self) -> Vec<Value> {
        Vec::new()
    }
    fn aoip_or_inspect(&self) -> Value {
        Value::Object(Map::new())
    }
    fn dante_ips(&self) -> Vec<String> {
        Vec::new()
    }
}
slot!(Nmos, nmos, provide_nmos, NMOS, NoNmos);

/// Pipeline inspector (apps/pipeline).
pub trait Pipeline: Send + Sync {
    fn inspect_full(&self) -> Value {
        Value::Null
    }
    fn inspect_aoip(&self, _use_cache: bool) -> Value {
        Value::Object(Map::new())
    }
}
slot!(Pipeline, pipeline, provide_pipeline, PIPELINE, NoPipeline);

/// CBC / Radio-Canada MXL test set (apps/cbc).
pub trait Cbc: Send + Sync {
    fn state(&self) -> Value {
        Value::Object(Map::new())
    }
    fn poll_generator(&self) {}
    fn post_raw(&self, _url: &str, _body: &[u8], _timeout: f64) -> Result<Vec<u8>, String> {
        Err("no CBC test set in this build".into())
    }
    fn get(&self, _url: &str, _headers: &[(&str, &str)], _timeout: f64) -> Result<Vec<u8>, String> {
        Err("no CBC test set in this build".into())
    }
}
slot!(Cbc, cbc, provide_cbc, CBC, NoCbc);

/// NTP (apps/ntp).
pub trait Ntp: Send + Sync {
    fn status(&self) -> Value {
        Value::Object(Map::new())
    }
}
slot!(Ntp, ntp, provide_ntp, NTP, NoNtp);
