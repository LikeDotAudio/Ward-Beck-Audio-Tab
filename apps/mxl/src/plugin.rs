// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.com - Ward-Beck.ca). All rights reserved.
//! The MXL engine plugs in /api/mxl* and offers the shared-memory data plane to the others.
//! The receivers' rings and audio ride along on the same routes (through their service).

use serde_json::{json, Map, Value};
use wbs_common::PcmChunk;
use wbs_plugin::reply::{json_error, loads_obj, qs_int, seg};
use wbs_plugin::services::{self, FlowInfo};
use wbs_plugin::{json_reply, Plugin, Reply, RouteResult};
use wbs_pyshim as py;

use crate::{engine, MxlEngine};

struct Mxl;

fn rings(query: &str) -> RouteResult {
    // Real ring-buffer state of every flow: write head, meter read head,
    // per-channel min/max bins and peak/RMS meters
    let bins = qs_int(query, "bins", 360)?.clamp(60, 720) as usize;
    let mut rep = engine().get_rings(bins);
    let rx = services::receivers();
    let (tp, others) = rx.rings(bins);
    if let Some(o) = rep.as_object_mut() {
        if let Some(tp) = tp.filter(py::truthy) {
            let flows = o.entry("flows").or_insert_with(|| json!([]));
            match flows.as_array_mut() {
                Some(a) => a.insert(0, tp),
                None => return Err("'flows' is not a list".into()),
            }
        }
        // The router's network / MoIP receivers ride after the TAP's own flows.
        if let Some(a) = o.get_mut("flows").and_then(|f| f.as_array_mut()) {
            a.extend(others.into_iter().filter(py::truthy));
        }
        o.insert("test_point".into(), rx.test_point_status());
        o.insert("receivers".into(), rx.receivers_status());
    }
    Ok(json_reply(&rep))
}

fn pcm_reply(res: Option<PcmChunk>) -> Reply {
    match res {
        None => Reply::Empty(404),
        Some(c) => Reply::Raw(
            200,
            vec![
                ("Content-Type".into(), "application/octet-stream".into()),
                ("Cache-Control".into(), "no-store".into()),
                ("X-Cursor".into(), c.cursor.to_string()),
                ("X-Channels".into(), c.channels.to_string()),
                ("X-Sample-Rate".into(), c.sample_rate.to_string()),
                ("X-Frames".into(), c.frames.to_string()),
                ("Access-Control-Expose-Headers".into(), "X-Cursor, X-Channels, X-Sample-Rate, X-Frames".into()),
            ],
            c.payload,
        ),
    }
}

fn slice(p: &str, query: &str) -> RouteResult {
    // Query slice: /api/mxl/flow/<flow_id>/slice?tail=479900&count=256
    let flow_id = seg(p, 4).unwrap_or("wbs-flow-master-stereo");
    let tail = qs_int(query, "tail", 479900)?;
    let count = qs_int(query, "count", 256)?.max(0) as usize;
    let eng = engine();
    let flow = eng.flow(flow_id).unwrap_or_else(|| eng.master_flow());
    let slice = flow.get_samples(tail, count);
    let Some(o) = slice.as_object() else { return Err("slice is not a dict".into()) };
    let preview = |k: &str| -> Result<Value, String> {
        match o.get(k) {
            Some(Value::Array(a)) => Ok(Value::Array(a.iter().take(8).cloned().collect())),
            _ => Err(format!("KeyError: '{k}'")),
        }
    };
    // Truncate samples in JSON preview to first 8 for readability
    let p0 = preview("samples_ch0")?;
    let p1 = preview("samples_ch1")?;
    let mut out = Map::new();
    for (k, v) in o {
        if k != "samples_ch0" && k != "samples_ch1" && k != "samples_ch0_preview" && k != "samples_ch1_preview" {
            out.insert(k.clone(), v.clone());
        }
    }
    out.insert("samples_ch0_preview".into(), p0);
    out.insert("samples_ch1_preview".into(), p1);
    Ok(json_reply(&Value::Object(out)))
}

impl Plugin for Mxl {
    fn name(&self) -> &'static str {
        "mxl"
    }

    fn get(&self, path: &str, query: &str) -> Option<RouteResult> {
        let p = path;
        Some(match p {
            "/api/mxl" => Ok(json_reply(&engine().get_telemetry())),
            "/api/mxl/rings" => rings(query),
            "/api/bitspitters" => Ok(json_reply(&crate::spitters::status_all())),
            p if spitter_size(p).is_some() => match crate::spitters::status(spitter_size(p).unwrap()) {
                Ok(v) => Ok(json_reply(&v)),
                Err(e) => Ok(json_error(404, &e)),
            },
            p if p == "/api/mxl/waveform" || (p.starts_with("/api/mxl/flow/") && p.contains("/waveform")) => {
                let flow_id = match seg(p, 4) {
                    Some(s) if s != "waveform" => s.to_string(),
                    _ => "wbs-flow-master-stereo".to_string(),
                };
                let eng = engine();
                Ok(json_reply(&match eng.flow(&flow_id) {
                    Some(f) => f.get_waveform_bins(),
                    None => eng.get_external_flow_waveform(&flow_id),
                }))
            }
            p if p.starts_with("/api/mxl/flow/") && p.ends_with("/pcm") => (|| {
                // /api/mxl/flow/<flow_id>/pcm?since=<cursor>&max=<frames> -> planar Float32 LE
                let flow_id = seg(p, 4).unwrap_or("");
                let since = qs_int(query, "since", -1)?;
                let max_frames = qs_int(query, "max", 9600)?.max(0) as usize;
                let res = services::receivers()
                    .pcm(flow_id, since, max_frames)
                    .unwrap_or_else(|| engine().get_flow_pcm(flow_id, since, max_frames));
                Ok(pcm_reply(res))
            })(),
            p if p.starts_with("/api/mxl/flow/") && p.ends_with("/levels") => {
                Ok(json_reply(&engine().get_flow_levels(seg(p, 4).unwrap_or(""))))
            }
            p if p.starts_with("/api/mxl/flow/") && p.contains("/slice") => slice(p, query),
            _ => return None,
        })
    }

    fn post(&self, path: &str, body: &str) -> Option<RouteResult> {
        match path {
            // Bit Spitter flows (1 / 2 / 4 / 8 / 16 ch): {active, pairs: [{pattern, level_db, tone_hz, ch_b, fault} × pairs]}
            p if spitter_size(p).is_some() => Some(loads_obj(body).map(|m| match crate::spitters::post(spitter_size(p).unwrap(), &m) {
                Ok(v) => json_reply(&v),
                Err(e) => json_error(400, &e),
            })),
            // POD-33 low tone flow (MXL67-3): {hz: 400 | 100}
            "/api/mxl/low_tone" => Some(loads_obj(body).map(|m| {
                let hz = m.get("hz").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                match crate::set_low_tone_hz(hz) {
                    Ok(h) => json_reply(&serde_json::json!({"hz": h})),
                    Err(e) => json_error(400, &e),
                }
            })),
            _ => None,
        }
    }
}

/// /api/bitspitter/<n> -> n (and the older /api/bitspitter8 -> 8).
fn spitter_size(path: &str) -> Option<usize> {
    if path == "/api/bitspitter8" {
        return Some(8);
    }
    path.strip_prefix("/api/bitspitter/")?.trim_end_matches('/').parse().ok()
}

fn flow_info_in(eng: &MxlEngine, flow_id: &str) -> Option<FlowInfo> {
    if let Some(f) = eng.flow(flow_id) {
        return Some(FlowInfo {
            flow_id: f.flow_id.clone(),
            label: f.label.clone(),
            domain_id: f.domain_id.clone(),
            channel_count: f.channel_count,
            sample_rate: f.sample_rate,
            quantum: f.quantum,
            buffer_length: f.buffer_length,
        });
    }
    eng.external_flow(flow_id).map(|f| FlowInfo {
        flow_id: f.flow_id,
        label: f.label,
        domain_id: f.domain_id,
        channel_count: f.channel_count,
        sample_rate: f.sample_rate,
        quantum: f.quantum,
        buffer_length: f.buffer_length,
    })
}

fn write_head_in(eng: &MxlEngine, flow_id: &str) -> Option<usize> {
    match eng.flow(flow_id) {
        Some(f) => Some(f.write_head()),
        None => eng.external_flow(flow_id)?.write_head(),
    }
}

fn read_planar_in(eng: &MxlEngine, flow_id: &str, start: usize, count: usize) -> Vec<Vec<f32>> {
    match eng.flow(flow_id) {
        Some(f) => f.read_planar(start, count),
        None => eng.external_flow(flow_id).map(|f| f.read_planar(start, count)).unwrap_or_default(),
    }
}

impl services::Mxl for Mxl {
    fn start(&self, source: &str) {
        engine().start(source)
    }
    fn stop(&self) {
        engine().stop()
    }
    fn set_source(&self, source: &str) {
        engine().set_source(source)
    }
    fn start_generator(&self, name: &str) {
        engine().start_generator(name)
    }
    fn stop_generators(&self) -> Vec<String> {
        engine().stop_generators()
    }
    fn telemetry(&self) -> Value {
        engine().get_telemetry()
    }
    fn scan_all_domains(&self) -> Value {
        engine().scan_all_domains()
    }
    fn domain_id(&self) -> String {
        engine().domain_id()
    }
    fn external_waveform(&self, flow_id: &str) -> Value {
        engine().get_external_flow_waveform(flow_id)
    }
    fn flow_ids(&self) -> Vec<String> {
        engine().flow_ids()
    }
    // Engine flows first, then a flow in any other domain (e.g. a CBC file player's), so the
    // receivers can take anything under the MXL root (util::shm_base(): /dev/shm/mxl on Linux,
    // <temp>/mxl on macOS / Windows).
    fn flow_info(&self, flow_id: &str) -> Option<FlowInfo> {
        flow_info_in(engine(), flow_id)
    }
    fn write_head(&self, flow_id: &str) -> Option<usize> {
        write_head_in(engine(), flow_id)
    }
    fn write_index(&self, flow_id: &str) -> Option<u64> {
        match engine().flow(flow_id) {
            Some(_) => None,
            None => engine().external_flow(flow_id)?.write_index(),
        }
    }
    fn read_planar(&self, flow_id: &str, start: usize, count: usize) -> Vec<Vec<f32>> {
        read_planar_in(engine(), flow_id, start, count)
    }
    fn flow_pcm(&self, flow_id: &str, since: i64, max_frames: usize) -> Option<PcmChunk> {
        engine().get_flow_pcm(flow_id, since, max_frames)
    }
    fn master_head(&self) -> (usize, u64) {
        let mf = engine().master_flow();
        (mf.write_head(), mf.write_seq())
    }
}

/// Plug the MXL engine into the host.
pub fn plug() {
    wbs_plugin::register(&Mxl);
    services::provide_mxl(Box::new(Mxl));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{fake_cbc_flow, scratch};
    use std::path::Path;

    #[test]
    fn service_falls_back_to_external_flows() {
        let base = scratch("plugin-ext");
        let dom = Path::new(&base).join("domain_1");
        fake_cbc_flow(&dom, "cbc-flow", "{\"id\":\"cbc-flow\",\"label\":\"Clip\",\"channel_count\":2}", 2, 100, 3 * 100 + 40);
        let e = MxlEngine::with_base("domain-wbs-dmf-01", &base);

        // engine flows win
        let m = flow_info_in(&e, "wbs-flow-master-stereo").unwrap();
        assert_eq!((m.channel_count, m.buffer_length, m.quantum), (2, 480000, 256));
        assert_eq!(write_head_in(&e, "wbs-flow-master-stereo"), Some(0));
        assert_eq!(read_planar_in(&e, "wbs-flow-master-stereo", 0, 4), vec![vec![0.0; 4]; 2]);

        // then any other domain's flow
        let x = flow_info_in(&e, "cbc-flow").unwrap();
        assert_eq!((x.flow_id.as_str(), x.label.as_str(), x.domain_id.as_str()), ("cbc-flow", "Clip", "domain_1"));
        assert_eq!((x.channel_count, x.sample_rate, x.quantum, x.buffer_length), (2, 48000, 480, 100));
        assert_eq!(write_head_in(&e, "cbc-flow"), Some(40));
        let p = read_planar_in(&e, "cbc-flow", 98, 4);
        assert_eq!(p, vec![vec![98.0, 99.0, 0.0, 1.0], vec![10098.0, 10099.0, 10000.0, 10001.0]]);

        // unknown flows: nothing
        assert!(flow_info_in(&e, "nope").is_none());
        assert_eq!(write_head_in(&e, "nope"), None);
        assert!(read_planar_in(&e, "nope", 0, 4).is_empty());
        e.stop();
        let _ = std::fs::remove_dir_all(&base);
    }
}
