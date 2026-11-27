// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.com - Ward-Beck.ca). All rights reserved.
//! The Bit Spitter's own flows: 1, 2, 4, 8 and 16 channels (`wbs-flow-bit-spitter-<n>`),
//! each a bank of ABS-1 pairs (Generators crate, `spitter8::SpitterBank`) written straight
//! into its MXL flow in the engine's domain. Every pair has its own source, level, tone,
//! channel B and line fault. Each flow has its own AES67 transmitter (aes67, the Bit
//! Spitter set), where the flow itself can be made GOOD, BAD or UGLY.
//!
//! Control: GET /api/bitspitters (all five), GET / POST /api/bitspitter/<n> (plugin.rs);
//! /api/bitspitter8 is /api/bitspitter/8.
//!   {"active": bool, "pairs": [{"pattern", "level_db", "tone_hz", "ch_b", "fault"} × pairs]}
//! A POST merges: any field may be left out, and `pairs` may be shorter than the flow's
//! pairs or hold null for a pair left as it is.
//!
//! The flows are created when the engine starts (or on the first POST) so they are listed
//! with the engine's own flows; a flow's synth only runs while it is active.

use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Instant;

use wbs_common as util;
use wbs_generators::spitter8::{pairs_for, Pair8, SpitterBank, SIZES};

use super::{
    clock_position, current_tai_ns, py_max, py_min, py_round, seq_max, seq_min, sleep_until, MxlFlow, CLOCK_SLIP_MAX_S,
    NUM_BINS,
};

/// The flow of the `n`-channel spitter.
pub fn flow_id(n: usize) -> String {
    format!("wbs-flow-bit-spitter-{n}")
}

/// Its label: "ABS-1 Bit Spitter (8 ch)".
pub fn label(n: usize) -> String {
    format!("ABS-1 Bit Spitter ({n} ch)")
}
const SAMPLE_RATE: u32 = 48000;
const QUANTUM: usize = 256;
const BUFFER_LENGTH: usize = 480_000; // 10 s, like the engine's other flows

const PATTERNS: [&str; 8] = ["sweep", "tone", "pink", "white", "brown", "walk1s", "prbs", "minmax"];
const CH_B: [&str; 4] = ["same", "cycle", "invert", "off"];
const FAULTS: [&str; 8] = ["clean", "noise", "badeq", "jitter", "drop", "slip", "biphase", "noclock"];

struct State {
    active: bool,
    pairs: Vec<Pair8>,
}

/// One spitter and its flow.
pub struct SpitterOut {
    channels: usize,
    flow: Arc<MxlFlow>,
    state: Mutex<State>,
    run: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
}

static OUTS: OnceLock<Vec<Arc<SpitterOut>>> = OnceLock::new();

/// The process-wide spitters (one per size), their flows under the engine's base and domain.
fn outs_for(base: &str, domain_id: &str) -> &'static [Arc<SpitterOut>] {
    OUTS.get_or_init(|| SIZES.iter().map(|&n| SpitterOut::new(base, domain_id, n)).collect())
}

fn outs() -> &'static [Arc<SpitterOut>] {
    let e = super::engine();
    outs_for(&e.shm_base_dir(), &e.domain_id())
}

fn out(n: usize) -> Result<&'static Arc<SpitterOut>, String> {
    outs().iter().find(|o| o.channels == n).ok_or_else(|| format!("no {n}-channel Bit Spitter (sizes: 1, 2, 4, 8, 16)"))
}

/// Hook: the flows, once they exist (engine lookups, flow lists, rings).
pub fn live_flows() -> Vec<Arc<MxlFlow>> {
    OUTS.get().map(|v| v.iter().map(|o| o.flow.clone()).collect()).unwrap_or_default()
}

/// Hook: the engine started. Make the flows and play on the spitters that were active.
pub fn on_engine_start(base: &str, domain_id: &str) {
    for o in outs_for(base, domain_id) {
        o.flow.ensure_open();
        if o.state.lock().unwrap().active {
            o.clone().start_worker();
        }
    }
}

/// Hook: the engine stopped. The synths stop; `active` is kept for the next start.
pub fn on_engine_stop() {
    for o in OUTS.get().map(|v| v.as_slice()).unwrap_or_default() {
        o.stop_worker();
        o.flow.close();
    }
}

/// GET /api/bitspitters
pub fn status_all() -> Value {
    json!({"sizes": SIZES, "spitters": outs().iter().map(|o| o.status()).collect::<Vec<_>>()})
}

/// GET /api/bitspitter/<n>
pub fn status(n: usize) -> Result<Value, String> {
    Ok(out(n)?.status())
}

/// POST /api/bitspitter/<n>
pub fn post(n: usize, body: &Map<String, Value>) -> Result<Value, String> {
    let o = out(n)?;
    o.apply(body)?;
    Ok(o.status())
}

impl SpitterOut {
    fn new(base: &str, domain_id: &str, channels: usize) -> Arc<SpitterOut> {
        Arc::new(SpitterOut {
            channels,
            flow: Arc::new(MxlFlow::new(
                base, domain_id, &flow_id(channels), &label(channels), SAMPLE_RATE, channels, BUFFER_LENGTH, QUANTUM, "generator",
            )),
            state: Mutex::new(State { active: false, pairs: vec![Pair8::default(); pairs_for(channels)] }),
            run: AtomicBool::new(false),
            thread: Mutex::new(None),
        })
    }

    fn running(&self) -> bool {
        self.thread.lock().unwrap().as_ref().map(|h| !h.is_finished()).unwrap_or(false)
    }

    fn status(&self) -> Value {
        let st = self.state.lock().unwrap();
        json!({
            "active": st.active,
            "running": self.running(),
            "flow_id": self.flow.flow_id,
            "label": self.flow.label,
            "channel_count": self.channels,
            "sample_rate": SAMPLE_RATE,
            "pairs": st.pairs.iter().map(pair_json).collect::<Vec<_>>(),
        })
    }

    /// Merge a POST body into the settings, then start / stop the synth.
    fn apply(self: &Arc<Self>, body: &Map<String, Value>) -> Result<(), String> {
        let active = {
            let mut st = self.state.lock().unwrap();
            let mut pairs = st.pairs.clone();
            match body.get("pairs") {
                None | Some(Value::Null) => {}
                Some(Value::Array(a)) => {
                    if a.len() > pairs.len() {
                        return Err(format!("pairs: at most {}", pairs.len()));
                    }
                    for (i, v) in a.iter().enumerate() {
                        match v {
                            Value::Null => {}
                            Value::Object(m) => merge_pair(&mut pairs[i], m).map_err(|e| format!("pairs[{i}].{e}"))?,
                            _ => return Err(format!("pairs[{i}]: not an object")),
                        }
                    }
                }
                Some(_) => return Err("pairs: not a list".into()),
            }
            match body.get("active") {
                None | Some(Value::Null) => {}
                Some(Value::Bool(b)) => st.active = *b,
                Some(_) => return Err("active: not true / false".into()),
            }
            st.pairs = pairs;
            st.active
        };
        if active {
            self.flow.ensure_open();
            self.clone().start_worker();
        } else if self.running() {
            self.stop_worker();
            self.flow.clear();
        }
        Ok(())
    }

    fn start_worker(self: Arc<Self>) {
        let mut slot = self.thread.lock().unwrap();
        if slot.as_ref().map(|h| !h.is_finished()).unwrap_or(false) {
            return;
        }
        self.run.store(true, Ordering::Release);
        let me = self.clone();
        *slot = Some(std::thread::spawn(move || me.worker()));
    }

    fn stop_worker(&self) {
        self.run.store(false, Ordering::Release);
        let h = self.thread.lock().unwrap().take();
        if let Some(h) = h {
            let _ = h.join();
        }
    }

    fn worker(&self) {
        let mut pairs = self.state.lock().unwrap().pairs.clone();
        let mut synth = SpitterBank::new(&pairs, self.channels, SAMPLE_RATE as f64);
        let mut buf: Vec<Vec<f64>> = Vec::new();
        let interval = QUANTUM as f64 / SAMPLE_RATE as f64;
        let start = Instant::now();
        let mut k = 0u64;
        while self.run.load(Ordering::Acquire) && !util::is_shutting_down() {
            {
                let st = self.state.lock().unwrap();
                for (i, p) in st.pairs.iter().enumerate() {
                    if *p != pairs[i] {
                        synth.set_pair(i, p);
                        pairs[i] = p.clone();
                    }
                }
            }
            synth.fill(&mut buf, QUANTUM);
            write_planar(&self.flow, &buf);
            self.flow.meter_read(4800);
            k += 1;
            sleep_until(start, k, interval);
        }
    }
}

fn pair_json(p: &Pair8) -> Value {
    json!({
        "pattern": p.pattern,
        "level_db": p.level_db,
        "tone_hz": p.tone_hz,
        "ch_b": p.ch_b,
        "fault": p.fault,
    })
}

fn merge_pair(p: &mut Pair8, m: &Map<String, Value>) -> Result<(), String> {
    let pick = |k: &str, allowed: &[&str]| -> Result<Option<String>, String> {
        match m.get(k) {
            None => Ok(None),
            Some(Value::String(s)) if allowed.contains(&s.as_str()) => Ok(Some(s.clone())),
            Some(v) => Err(format!("{k}: {v} is not one of {}", allowed.join(" | "))),
        }
    };
    if let Some(s) = pick("pattern", &PATTERNS)? {
        p.pattern = s;
    }
    if let Some(s) = pick("ch_b", &CH_B)? {
        p.ch_b = s;
    }
    if let Some(s) = pick("fault", &FAULTS)? {
        p.fault = s;
    }
    match m.get("level_db") {
        None => {}
        Some(Value::Null) => p.level_db = None,
        Some(v) => p.level_db = Some(v.as_f64().ok_or("level_db: not a number or null")?.clamp(-100.0, 0.0)),
    }
    if let Some(v) = m.get("tone_hz") {
        p.tone_hz = v.as_f64().ok_or("tone_hz: not a number")?.clamp(10.0, 20_000.0);
    }
    Ok(())
}

/// `MxlFlow::write_frames` for any number of channels: planar samples into the ring,
/// the first two channels into the waveform bins (a 1-channel flow: its channel in both),
/// then the write head and the header.
fn write_planar(flow: &MxlFlow, chans: &[Vec<f64>]) {
    flow.ensure_open();
    let n = chans.iter().map(|c| c.len()).min().unwrap_or(0);
    if n == 0 || chans.is_empty() {
        return;
    }
    let _w = flow.write_lock.lock().unwrap();
    let maps = flow.maps.read().unwrap();
    let buf_len = flow.buffer_length;
    let mut head = flow.write_head.load(Ordering::Acquire);
    if flow.clock_locked.load(Ordering::Relaxed) {
        let now = clock_position(current_tai_ns(), flow.sample_rate, buf_len);
        let want = (now + buf_len - n % buf_len) % buf_len;
        let slip = (head as i64 - want as i64).rem_euclid(buf_len as i64);
        let slip = slip.min(buf_len as i64 - slip) as f64 / flow.sample_rate as f64;
        if slip > CLOCK_SLIP_MAX_S {
            head = want;
        }
    }
    for (c, data) in chans.iter().enumerate().take(flow.channel_count) {
        let data = &data[..n];
        if head + n <= buf_len {
            flow.write_seg(&maps, c, head, data);
        } else {
            let l1 = buf_len - head;
            flow.write_seg(&maps, c, head, &data[..l1]);
            flow.write_seg(&maps, c, 0, &data[l1..]);
        }
    }
    {
        let mut b = flow.bins.lock().unwrap();
        let fpb = buf_len as f64 / NUM_BINS as f64;
        let mut i = 0usize;
        while i < n {
            let pos = (head + i) % buf_len;
            let bi = ((pos as f64 / fpb) as usize) % NUM_BINS;
            let bin_end = buf_len.min(((bi + 1) as f64 * fpb) as usize);
            let run = (n - i).min((bin_end as i64 - pos as i64).max(1) as usize);
            let s0 = &chans[0][i..i + run];
            let s1 = &chans[chans.len().min(2) - 1][i..i + run];
            if bi as i64 != b.last_bin {
                b.ch0_min[bi] = 0.0;
                b.ch0_max[bi] = 0.0;
                b.ch1_min[bi] = 0.0;
                b.ch1_max[bi] = 0.0;
                b.last_bin = bi as i64;
            }
            b.ch0_min[bi] = py_round(py_min(b.ch0_min[bi], seq_min(s0)), 5);
            b.ch0_max[bi] = py_round(py_max(b.ch0_max[bi], seq_max(s0)), 5);
            b.ch1_min[bi] = py_round(py_min(b.ch1_min[bi], seq_min(s1)), 5);
            b.ch1_max[bi] = py_round(py_max(b.ch1_max[bi], seq_max(s1)), 5);
            i += run;
        }
    }
    let new_head = (head + n) % buf_len;
    flow.write_head.store(new_head, Ordering::Release);
    let seq = flow.write_seq.fetch_add(1, Ordering::AcqRel) + 1;
    let tai = current_tai_ns();
    unsafe {
        std::ptr::copy_nonoverlapping((new_head as u32).to_le_bytes().as_ptr(), maps.hdr_w.add(8), 4);
        std::ptr::copy_nonoverlapping(seq.to_le_bytes().as_ptr(), maps.hdr_w.add(16), 8);
        std::ptr::copy_nonoverlapping(tai.to_le_bytes().as_ptr(), maps.hdr_w.add(24), 8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A scratch folder (never /dev/shm/mxl).
    fn scratch(name: &str) -> String {
        let root = std::env::var("WBS_TEST_SCRATCH").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
        let p = std::path::Path::new(&root).join(format!("mxl-spitters-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn eight_channels_into_the_ring() {
        let base = scratch("ring");
        let o = SpitterOut::new(&base, "dom", 8);
        let chans: Vec<Vec<f64>> = (0..8).map(|c| vec![(c + 1) as f64 / 10.0; 300]).collect();
        write_planar(&o.flow, &chans);
        assert_eq!(o.flow.write_head(), 300);
        let planar = o.flow.read_planar(0, 300);
        assert_eq!(planar.len(), 8);
        for (c, ch) in planar.iter().enumerate() {
            assert!(ch.iter().all(|&v| v == ((c + 1) as f64 / 10.0) as f32), "channel {c}");
        }
        let def: Value = serde_json::from_str(&std::fs::read_to_string(format!("{base}/dom/{}/flow_def.json", flow_id(8))).unwrap()).unwrap();
        assert_eq!((def["channel_count"].as_u64(), def["label"].as_str()), (Some(8), Some(label(8).as_str())));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn post_merges_and_checks() {
        let base = scratch("post");
        let o = SpitterOut::new(&base, "dom", 8);
        o.apply(&obj(json!({"pairs": [null, {"pattern": "tone", "level_db": null, "tone_hz": 400, "ch_b": "invert", "fault": "jitter"}]})))
            .unwrap();
        let s = o.status();
        assert_eq!(s["active"], false);
        assert_eq!(s["pairs"].as_array().unwrap().len(), 4);
        assert_eq!(s["pairs"][0]["pattern"], Pair8::default().pattern.as_str());
        assert_eq!(s["pairs"][1], json!({"pattern": "tone", "level_db": null, "tone_hz": 400.0, "ch_b": "invert", "fault": "jitter"}));
        assert!(o.apply(&obj(json!({"pairs": [{"pattern": "file"}]}))).is_err());
        assert!(o.apply(&obj(json!({"pairs": [{}, {}, {}, {}, {}]}))).is_err());
        assert!(o.apply(&obj(json!({"active": "yes"}))).is_err());
        assert!(o.apply(&obj(json!({"pairs": [{"level_db": 6}]}))).is_ok());
        assert_eq!(o.status()["pairs"][0]["level_db"], 0.0, "clamped to 0 dBFS");
        assert_eq!(o.status()["pairs"][1]["tone_hz"], 400.0, "a bad POST changes nothing");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn activate_plays_and_deactivate_silences() {
        let base = scratch("run");
        let o = SpitterOut::new(&base, "dom", 8);
        let tone = json!({"pattern": "tone", "level_db": 0, "tone_hz": 1000, "ch_b": "same", "fault": "clean"});
        let off = json!({"pattern": "tone", "level_db": null});
        o.apply(&obj(json!({"active": true, "pairs": [tone, off, tone, {"ch_b": "off", "pattern": "minmax"}]}))).unwrap();
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(o.status()["running"], true);
        let w = o.flow.write_head();
        assert!(w >= 2 * QUANTUM, "write head {w}");
        let p = o.flow.read_planar(0, w);
        let peak = |c: usize| p[c].iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!(peak(0) > 0.99 && peak(1) > 0.99 && peak(4) > 0.99, "tone at 0 dBFS");
        assert_eq!(peak(2) + peak(3), 0.0, "AES 3/4 level OFF");
        assert!((peak(6) - 0.95).abs() < 1e-6 && peak(7) == 0.0, "AES 7/8 min/max, B off");
        o.apply(&obj(json!({"active": false}))).unwrap();
        assert_eq!(o.status()["running"], false);
        assert!(o.flow.read_planar(0, w).iter().all(|c| c.iter().all(|&v| v == 0.0)), "ring cleared");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn every_size_has_its_own_flow() {
        let base = scratch("sizes");
        for &n in &SIZES {
            let o = SpitterOut::new(&base, "dom", n);
            let tone = json!({"pattern": "tone", "level_db": 0, "tone_hz": 1000, "ch_b": "same", "fault": "clean"});
            let pairs: Vec<Value> = (0..pairs_for(n)).map(|_| tone.clone()).collect();
            o.apply(&obj(json!({"active": true, "pairs": pairs}))).unwrap();
            std::thread::sleep(Duration::from_millis(60));
            let s = o.status();
            assert_eq!((s["channel_count"].as_u64(), s["pairs"].as_array().unwrap().len()), (Some(n as u64), pairs_for(n)));
            assert_eq!(s["flow_id"], format!("wbs-flow-bit-spitter-{n}"));
            let w = o.flow.write_head();
            let p = o.flow.read_planar(0, w);
            assert_eq!(p.len(), n);
            assert!(p.iter().all(|c| c.iter().fold(0f32, |m, v| m.max(v.abs())) > 0.99), "{n} ch: every channel plays");
            let too_many: Vec<Value> = (0..pairs_for(n) + 1).map(|_| json!({})).collect();
            assert!(o.apply(&obj(json!({"pairs": too_many}))).is_err());
            o.apply(&obj(json!({"active": false}))).unwrap();
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
