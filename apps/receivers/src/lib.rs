// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.com - Ward-Beck.ca). All rights reserved.
//! Port of test_point.py: one test point at a time (hw / pw / mxl / aoip), its own ring + meters.
//! CONTRACT: keep these pub signatures.
//!
//! One test point is active at a time, like the probe on a test-point jack. Whatever it
//! is clipped onto is brought into its own ring buffer sized to 10 x that point's packet
//! time (AES67 / RAVENNA / Dante-AES67 streams: a fixed 10 s ring), and metered per
//! channel (peak / RMS) from the ring:
//!
//!   hw    ALSA card          -> its PipeWire source (or sink monitor), parec
//!                               packet = PipeWire graph quantum
//!   pw    PipeWire node/port -> source, sink monitor, playback stream (monitor-stream)
//!                               or, for a capture stream, the node feeding it
//!                               packet = PipeWire graph quantum
//!   mxl   MXL flow           -> read behind the flow's write head (shared memory)
//!                               packet = flow quantum (grain)
//!   aoip  AES67 / RAVENNA RTP-> joins the SDP multicast group, decodes L24 / L16
//!                               packet = measured RTP payload (ptime x rate)
//!
//! Nothing is synthesized: a point that cannot be reached reports why.

// Plugged into the host: routes, services and start-up (plugin.rs).
pub mod ams81;
pub mod plugin;
pub use plugin::plug;

use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::Read;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use wbs_pyshim::sock as py;
use wbs_common::{now_f64, run_cmd, PcmChunk};
use wbs_plugin::services::{FlowInfo, Mxl};

pub const RING_PACKETS: usize = 10;
pub const RING_SECONDS: f64 = 10.0; // every test point (ALSA, PipeWire, MXL, AoIP): 10 s ring
pub const MAX_CHANNELS: usize = 16;
pub const FLOW_ID: &str = "wbs-test-point";

/// One receiver per slot, each with its own ring: the test point (slot 0) clips onto
/// anything; the router's dedicated network and MoIP inputs (slots 1, 2) receive RTP streams.
pub struct Slot {
    pub name: &'static str,
    pub flow_id: &'static str,
    /// the AMS-81 stereo fold-down of this receiver ("" = none: the test point)
    pub fold_flow_id: &'static str,
    pub title: &'static str,
    pub kind: &'static str,
}
pub const SLOTS: [Slot; 3] = [
    Slot { name: "testpoint", flow_id: FLOW_ID, fold_flow_id: "", title: "TEST POINT", kind: "testpoint" },
    Slot {
        name: "network",
        flow_id: "wbs-network-rx",
        fold_flow_id: "wbs-network-ams81",
        title: "NETWORK RX",
        kind: "receiver",
    },
    Slot { name: "moip", flow_id: "wbs-moip-rx", fold_flow_id: "wbs-moip-ams81", title: "MOIP RX", kind: "receiver" },
];
pub const TEST_POINT: usize = 0;

pub fn slot_named(name: &str) -> Option<usize> {
    SLOTS.iter().position(|s| s.name == name)
}

pub fn slot_of_flow(flow_id: &str) -> Option<usize> {
    SLOTS.iter().position(|s| s.flow_id == flow_id)
}

/// The receiver whose AMS-81 fold-down is `flow_id`.
pub fn slot_of_fold(flow_id: &str) -> Option<usize> {
    SLOTS.iter().position(|s| !s.fold_flow_id.is_empty() && s.fold_flow_id == flow_id)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A Python exception: "<Class>: <message>" in the point's error.
#[derive(Debug)]
pub struct PyErr {
    pub class: &'static str,
    pub msg: String,
}

impl PyErr {
    fn lookup(msg: impl Into<String>) -> Self {
        PyErr { class: "LookupError", msg: msg.into() }
    }
}

impl From<std::io::Error> for PyErr {
    fn from(e: std::io::Error) -> Self {
        PyErr { class: py::exc_class(&e), msg: py::os_error(&e) }
    }
}

impl From<(&'static str, String)> for PyErr {
    fn from(e: (&'static str, String)) -> Self {
        PyErr { class: e.0, msg: e.1 }
    }
}

fn run(args: &[&str], timeout: f64) -> String {
    run_cmd(args, Duration::from_secs_f64(timeout)).unwrap_or_default()
}

fn pw_objects() -> Vec<Value> {
    serde_json::from_str::<Value>(&run(&["pw-dump"], 4.0))
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

fn ufw_enabled() -> bool {
    std::fs::read_to_string("/etc/ufw/ufw.conf")
        .map(|t| t.lines().any(|l| py::strip(l).to_uppercase() == "ENABLED=YES"))
        .unwrap_or(false)
}

fn route_iface(group: &str) -> String {
    let out = run(&["ip", "-o", "route", "get", group], 3.0);
    let parts = py::split_ws(&out);
    match parts.iter().position(|&p| p == "dev") {
        Some(i) if i + 1 < parts.len() => parts[i + 1].to_string(),
        Some(_) => String::new(),
        None => String::new(),
    }
}

fn mcast_count(iface: &str) -> i64 {
    std::fs::read_to_string(format!("/sys/class/net/{iface}/statistics/multicast"))
        .ok()
        .and_then(|t| py::parse_int(&t))
        .unwrap_or(-1)
}

/// The PipeWire graph quantum in frames (the PipeWire 'packet') from `pw-metadata` output.
pub fn parse_graph_quantum(out: &str) -> i64 {
    let mut force = None;
    let mut quantum = None;
    for (key, slot) in [("clock.force-quantum", &mut force), ("clock.quantum", &mut quantum)] {
        if let Some(i) = out.find(&format!("'{key}'")) {
            if let Some((_, rest)) = out[i..].split_once("value:'") {
                if let Some(v) = py::parse_int(rest.split('\'').next().unwrap_or("")) {
                    *slot = Some(v);
                }
            }
        }
    }
    let mut q = force.filter(|&v| v != 0).unwrap_or(0);
    if q <= 0 {
        q = quantum.filter(|&v| v != 0).unwrap_or(1024);
    }
    q.clamp(32, 16384)
}

fn graph_quantum() -> usize {
    parse_graph_quantum(&run(&["pw-metadata", "-n", "settings", "0"], 3.0)) as usize
}

// ---------------------------------------------------------------------------
// Ring
// ---------------------------------------------------------------------------

/// Planar Float32 ring per channel: a fixed duration (live test points: 10 s), or
/// RING_PACKETS x packet_frames frames when seconds is 0. Waveform min/max bins and peak/RMS meters are maintained
/// as each packet is written, so a 480,000-frame x 8ch ring snapshots in O(bins).
pub struct TapRing {
    pub channels: usize,
    pub packet_frames: usize,
    pub sample_rate: u32,
    pub length: usize,
    pub data: Vec<Vec<f32>>,
    pub head: usize,
    pub frames: u64,
    pub packets: u64,
    nb: usize,
    bmin: Vec<Vec<f32>>,
    bmax: Vec<Vec<f32>>,
    last_bin: i64,
    pub peak: Vec<f64>,
    pub rms: Vec<f64>,
    peak_fall: f64,
    rms_k: f64,
}

impl TapRing {
    pub const NUM_BINS: usize = 720;

    pub fn new(channels: usize, packet_frames: usize, sample_rate: u32, seconds: f64) -> Self {
        let length = if seconds > 0.0 { (seconds * sample_rate as f64) as usize } else { RING_PACKETS * packet_frames };
        let nb = Self::NUM_BINS.min(length);
        // ballistics: peak falls back over ~1.5 s, RMS integrates over ~300 ms
        let pkt_s = packet_frames.max(1) as f64 / sample_rate as f64;
        TapRing {
            channels,
            packet_frames,
            sample_rate,
            length,
            data: vec![vec![0.0; length]; channels],
            head: 0,
            frames: 0,
            packets: 0,
            nb,
            bmin: vec![vec![0.0; nb]; channels],
            bmax: vec![vec![0.0; nb]; channels],
            last_bin: -1,
            peak: vec![0.0; channels],
            rms: vec![0.0; channels],
            peak_fall: 0.92f64.powf(pkt_s / 0.1),
            rms_k: 1.0 - (-pkt_s / 0.3).exp(),
        }
    }

    fn bin_of(&self, pos: usize) -> usize {
        pos * self.nb / self.length
    }

    pub fn write(&mut self, planar: &[Vec<f32>]) {
        let mut n = planar.first().map(|p| p.len()).unwrap_or(0);
        if n == 0 || self.length == 0 {
            return;
        }
        let mut off = 0;
        if n > self.length {
            off = n - self.length;
            n = self.length;
        }
        let (h, l) = (self.head, self.length);
        let first = n.min(l - h);
        for c in 0..self.channels {
            let src = &planar[c][off..off + n];
            self.data[c][h..h + first].copy_from_slice(&src[..first]);
            if first < n {
                self.data[c][..n - first].copy_from_slice(&src[first..n]);
            }
            // meters from this packet
            let pk = py::max2(py::max_of(src), -py::min_of(src));
            let ms = py::fsum_neumaier(src.iter().map(|&v| (v as f64) * (v as f64))) / n as f64;
            self.peak[c] = py::max2(pk, self.peak[c] * self.peak_fall);
            self.rms[c] += (ms.sqrt() - self.rms[c]) * self.rms_k;
        }
        // bins: a bin is reset the first time the write head enters it
        let mut i = 0;
        while i < n {
            let pos = (h + i) % l;
            let b = self.bin_of(pos);
            let bin_end = l.min(((b + 1) * l).div_ceil(self.nb));
            let run = (n - i).min(bin_end.saturating_sub(pos).max(1));
            let fresh = b as i64 != self.last_bin;
            self.last_bin = b as i64;
            for c in 0..self.channels {
                let seg = &planar[c][off + i..off + i + run];
                let (lo, hi) = (py::min_of(seg) as f32, py::max_of(seg) as f32);
                if fresh {
                    self.bmin[c][b] = lo;
                    self.bmax[c][b] = hi;
                } else {
                    if lo < self.bmin[c][b] {
                        self.bmin[c][b] = lo;
                    }
                    if hi > self.bmax[c][b] {
                        self.bmax[c][b] = hi;
                    }
                }
            }
            i += run;
        }
        self.head = (h + n) % l;
        self.frames += n as u64;
        self.packets += 1;
    }

    pub fn snapshot(&self, bins: usize) -> Vec<Value> {
        let step = (self.nb / bins.max(1)).max(1);
        let db = |v: f64| if v > 1e-6 { py::round(20.0 * v.log10(), 2) } else { -120.0 };
        (0..self.channels)
            .map(|c| {
                let (mins, maxs) = (&self.bmin[c], &self.bmax[c]);
                let lo: Vec<f64> = (0..self.nb).step_by(step).map(|i| py::round(py::min_of(&mins[i..(i + step).min(self.nb)]), 5)).collect();
                let hi: Vec<f64> = (0..self.nb).step_by(step).map(|i| py::round(py::max_of(&maxs[i..(i + step).min(self.nb)]), 5)).collect();
                let mut m = Map::new();
                m.insert("min".into(), json!(lo));
                m.insert("max".into(), json!(hi));
                m.insert("peak_db".into(), json!(db(self.peak[c])));
                m.insert("rms_db".into(), json!(db(self.rms[c])));
                Value::Object(m)
            })
            .collect()
    }

    /// New ring audio since `since` (planar f32 LE), newest `max_frames` at most.
    pub fn pcm_since(&self, since: i64, max_frames: usize) -> Option<PcmChunk> {
        let l = self.length as i64;
        if l == 0 {
            return None;
        }
        let w = self.head as i64;
        if since < 0 {
            return Some(PcmChunk { payload: vec![], cursor: w, channels: self.channels, sample_rate: self.sample_rate, frames: 0 });
        }
        let mut since = since;
        let mut avail = (w - since).rem_euclid(l);
        if avail > max_frames as i64 {
            since = (w - max_frames as i64).rem_euclid(l);
            avail = max_frames as i64;
        }
        // Python list slicing (clamped) of each channel's ring
        let sl = |d: &[f32], a: i64, b: i64| -> Vec<f32> {
            let a = a.clamp(0, l) as usize;
            let b = b.clamp(0, l) as usize;
            if a < b { d[a..b].to_vec() } else { vec![] }
        };
        let mut out = Vec::new();
        for d in &self.data {
            let seg = if since + avail <= l {
                sl(d, since, since + avail)
            } else {
                let mut s = sl(d, since, l);
                s.extend(sl(d, 0, avail - (l - since)));
                s
            };
            for v in seg {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        Some(PcmChunk { payload: out, cursor: w, channels: self.channels, sample_rate: self.sample_rate, frames: avail as usize })
    }
}

// ---------------------------------------------------------------------------
// MXL reader: follows one flow's write head, through its writer stopping and restarting
// ---------------------------------------------------------------------------

/// The writer is called stopped when its head has not moved for this long.
pub const MXL_STALL: Duration = Duration::from_secs(1);
/// The flow's geometry (ring length, channels, rate, grain) is looked up again this often.
const MXL_RECHECK: Duration = Duration::from_secs(2);

/// What one poll of an MXL flow produced.
#[derive(Default)]
struct MxlStep {
    /// (re)resolved flow: the slot takes its geometry
    flow: Option<FlowInfo>,
    /// new (status, hint)
    status: Option<(String, Option<String>)>,
    /// new frames, planar
    audio: Option<Vec<Vec<f32>>>,
    /// packets the reader fell too far behind to read (overrun)
    lost: u64,
}

/// Reads behind one flow's write head. The slot stays on the flow id: when the flow
/// disappears or its head stops, the reader waits and resumes on its own once the writer
/// is back, resolving the flow again (ring length / channels may change) and starting at the
/// new head rather than replaying a stale ring.
struct MxlReader {
    id: String,
    info: Option<FlowInfo>,
    live: bool,
    shown: Option<(String, Option<String>)>,
    cursor: usize,
    index: Option<u64>,
    last_move: Instant,
    last_read: Instant,
    last_check: Instant,
    stalls: u64,
    restarts: u64,
}

const MXL_GONE: &str = "writer gone: the flow left /dev/shm/mxl; resumes when it is back";
const MXL_STOPPED: &str = "writer stopped: its write head has not moved for over 1 s; resumes when it moves";

impl MxlReader {
    fn new(id: &str, now: Instant) -> Self {
        MxlReader {
            id: id.to_string(),
            info: None,
            live: false,
            shown: None,
            cursor: 0,
            index: None,
            last_move: now,
            last_read: now,
            last_check: now,
            stalls: 0,
            restarts: 0,
        }
    }

    /// Poll interval: one grain while live, 50 ms while waiting for the writer.
    fn interval(&self) -> Duration {
        match &self.info {
            Some(f) if self.live && f.sample_rate > 0 => {
                Duration::from_secs_f64((f.quantum.max(1) as f64 / f.sample_rate as f64).clamp(0.0005, 0.05))
            }
            _ => Duration::from_millis(50),
        }
    }

    /// Status into the step, only when it changes.
    fn show(&mut self, out: &mut MxlStep, status: &str, hint: Option<&str>) {
        let st = (status.to_string(), hint.map(str::to_string));
        if self.shown.as_ref() != Some(&st) {
            self.shown = Some(st.clone());
            out.status = Some(st);
        }
    }

    fn wait(&mut self, out: &mut MxlStep, hint: &str) {
        if self.live {
            self.live = false;
            self.stalls += 1;
        }
        self.show(out, "waiting", Some(hint));
    }

    /// Start (again) at the writer's head: nothing stale is read.
    fn resume(&mut self, out: &mut MxlStep, w: usize, index: Option<u64>, now: Instant) {
        self.live = true;
        self.cursor = w;
        self.index = index;
        self.last_move = now;
        self.last_read = now;
        self.show(out, "running", None);
    }

    fn poll(&mut self, mxl: &dyn Mxl, now: Instant) -> MxlStep {
        let mut out = MxlStep::default();
        let Some(head) = mxl.write_head(&self.id) else {
            self.info = None;
            self.wait(&mut out, MXL_GONE);
            return out;
        };
        let idx = mxl.write_index(&self.id);
        // (re)resolve: first time, after the flow was gone, and every MXL_RECHECK
        if self.info.is_none() || now.duration_since(self.last_check) >= MXL_RECHECK {
            self.last_check = now;
            let Some(f) = mxl.flow_info(&self.id).filter(|f| f.buffer_length > 0 && f.sample_rate > 0) else {
                self.info = None;
                self.wait(&mut out, MXL_GONE);
                return out;
            };
            let geom = |f: &FlowInfo| (f.buffer_length, f.channel_count, f.sample_rate, f.quantum);
            let was = self.info.as_ref().map(geom);
            if was != Some(geom(&f)) {
                if was.is_some() {
                    self.restarts += 1; // same id, new ring: the writer restarted in place
                }
                let w = head % f.buffer_length;
                self.info = Some(f.clone());
                out.flow = Some(f);
                self.resume(&mut out, w, idx, now);
                return out;
            }
        }
        let Some(f) = self.info.clone() else { return out };
        let (l, q) = (f.buffer_length, f.quantum.max(1));
        let w = head % l;
        let avail = (w + l - self.cursor % l) % l;
        let expect = now.duration_since(self.last_read).as_secs_f64() * f.sample_rate as f64;
        // the head index jumping = the writer restarted (backwards, or further ahead than the
        // elapsed time explains); ahead by more than a ring in time = we fell behind (overrun)
        let (jumped, lost) = match (idx, self.index) {
            (Some(i), Some(p)) if i < p => (true, 0),
            (Some(i), Some(p)) if i - p > l as u64 => {
                let d = i - p;
                if d as f64 <= expect * 2.0 + q as f64 { (true, (d - l as u64).div_ceil(q as u64).max(1)) } else { (true, 0) }
            }
            (Some(_), Some(_)) => (false, 0),
            // no index: a head far beyond what the elapsed time allows is a restart
            _ => (avail > l / 4 && avail as f64 > expect * 4.0 + 4.0 * q as f64, 0),
        };
        if jumped {
            if lost == 0 {
                self.restarts += 1;
            }
            out.lost = lost;
            self.resume(&mut out, w, idx, now);
            return out;
        }
        if avail == 0 {
            if self.live && now.duration_since(self.last_move) > MXL_STALL {
                self.wait(&mut out, MXL_STOPPED);
            }
            return out;
        }
        if !self.live {
            self.resume(&mut out, w, idx, now);
            return out;
        }
        let n = avail.min((RING_SECONDS * f.sample_rate as f64) as usize);
        let start = (w + l - n % l) % l;
        let planar = mxl.read_planar(&self.id, start, n);
        if planar.len() == f.channel_count && planar.iter().all(|p| p.len() == n) {
            out.audio = Some(planar);
        }
        self.cursor = w;
        self.index = idx;
        self.last_move = now;
        self.last_read = now;
        out
    }
}

// ---------------------------------------------------------------------------
// Test point
// ---------------------------------------------------------------------------

struct TpState {
    source: String, // resolved description of what is tapped
    status: String,
    error: Option<String>,
    hint: Option<String>,
    fix: Value, // {title, why, options:[{label, note, command}]}: only the user can run it
    ring: Option<TapRing>,
    fold: Option<TapRing>, // receivers: the AMS-81 stereo fold-down of `ring`
    sample_rate: u32,
    channels: usize,
    packet_frames: usize,
    lost: u64,
    group: Option<String>, // aoip: the multicast group being received
}

impl TpState {
    /// One packet into the ring; a receiver also folds it (AMS-81) into its stereo `fold`
    /// ring, kept at the ring's packet size and rate.
    fn write_planar(&mut self, slot: usize, planar: &[Vec<f32>]) {
        let Some(r) = self.ring.as_mut() else { return };
        // Module inserts (M466 …) sit at the receiver's input: everything after hears them
        let ins = wbs_plugin::services::inserts();
        let processed;
        let planar = if slot != TEST_POINT && ins.active(SLOTS[slot].name) {
            let mut p = planar.to_vec();
            ins.process(SLOTS[slot].name, r.sample_rate, &mut p);
            processed = p;
            &processed[..]
        } else {
            planar
        };
        r.write(planar);
        if SLOTS[slot].fold_flow_id.is_empty() {
            return;
        }
        let (pf, sr) = (r.packet_frames, r.sample_rate);
        if !self.fold.as_ref().is_some_and(|f| f.packet_frames == pf && f.sample_rate == sr) {
            self.fold = Some(TapRing::new(2, pf, sr, RING_SECONDS));
        }
        let lr = ams81::settings()[slot].fold(planar);
        if let Some(f) = self.fold.as_mut() {
            f.write(&lr);
        }
    }
}

struct TestPoint {
    slot: usize,
    kind: String,
    reference: String,
    label: String,
    st: Mutex<TpState>,
    stop: AtomicBool,
    child_pid: Mutex<Option<u32>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    #[allow(dead_code)]
    started: f64,
}

type Shared = Arc<TestPoint>;

impl TestPoint {
    fn new(slot: usize, kind: &str, reference: &str, label: &str) -> Shared {
        Arc::new(TestPoint {
            slot,
            kind: kind.to_string(),
            reference: reference.to_string(),
            label: if label.is_empty() { reference.to_string() } else { label.to_string() },
            st: Mutex::new(TpState {
                source: String::new(),
                status: "starting".into(),
                error: None,
                hint: None,
                fix: Value::Null,
                ring: None,
                fold: None,
                sample_rate: 48000,
                channels: 2,
                packet_frames: 0,
                lost: 0,
                group: None,
            }),
            stop: AtomicBool::new(false),
            child_pid: Mutex::new(None),
            thread: Mutex::new(None),
            started: now_f64(),
        })
    }

    // -- lifecycle --
    fn start(self: &Shared) {
        let me = self.clone();
        let h = std::thread::Builder::new().name(format!("{}-{}", SLOTS[self.slot].name, self.kind)).spawn(move || me.run());
        if let Ok(h) = h {
            *lock(&self.thread) = Some(h);
        }
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(pid) = *lock(&self.child_pid) {
            wbs_sys::kill(pid, wbs_sys::SIGTERM);
        }
        let h = lock(&self.thread).take();
        if let Some(h) = h {
            let end = Instant::now() + Duration::from_millis(1500);
            while !h.is_finished() && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(10));
            }
            if h.is_finished() {
                let _ = h.join();
            }
        }
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn fail(&self, msg: String, hint: Option<String>) {
        let mut s = lock(&self.st);
        s.status = "error".into();
        s.error = Some(msg);
        s.hint = hint;
    }

    fn run(self: Arc<Self>) {
        let r = match self.kind.as_str() {
            "hw" | "pw" => self.run_pipewire(),
            "mxl" => self.run_mxl(),
            "aoip" => self.run_rtp(),
            _ => {
                self.fail(format!("'{}' points cannot be metered", self.kind), None);
                Ok(())
            }
        };
        if let Err(e) = r {
            self.fail(format!("{}: {}", e.class, e.msg), None);
        }
    }

    // -- PipeWire / ALSA --
    fn resolve_pipewire(&self) -> Result<(PwTarget, Vec<Value>), PyErr> {
        let objs = pw_objects();
        resolve_pipewire_in(&self.kind, &self.reference, objs)
    }

    fn run_pipewire(&self) -> Result<(), PyErr> {
        let ((dev_args, desc, ch), _objs) = self.resolve_pipewire()?;
        let channels = (ch.min(MAX_CHANNELS as i64)).max(1) as usize;
        let packet_frames = graph_quantum();
        let sample_rate;
        {
            let mut s = lock(&self.st);
            s.channels = channels;
            s.packet_frames = packet_frames;
            s.source = desc.clone();
            s.ring = Some(TapRing::new(channels, packet_frames, s.sample_rate, RING_SECONDS));
            sample_rate = s.sample_rate;
        }
        let lat_ms = ((1000 * packet_frames as u64 / sample_rate as u64) as i64).max(1);
        let mut args: Vec<String> = dev_args;
        args.push("--format=float32le".into());
        args.push(format!("--rate={sample_rate}"));
        args.push(format!("--channels={channels}"));
        args.push("--raw".into());
        args.push(format!("--latency-msec={lat_ms}"));
        let mut child: Child = Command::new("parec")
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                let mut p = PyErr::from(e);
                if p.class == "FileNotFoundError" {
                    p.msg = format!("{}: 'parec'", p.msg);
                }
                p
            })?;
        *lock(&self.child_pid) = Some(child.id());
        if self.stopped() {
            wbs_sys::kill(child.id(), wbs_sys::SIGTERM);
        }
        lock(&self.st).status = "running".into();
        let chunk = packet_frames * channels * 4;
        let n = channels;
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut raw = vec![0u8; chunk];
        while !self.stopped() {
            // BufferedReader.read(n): until n bytes or EOF
            let mut got = 0;
            while got < chunk {
                match stdout.read(&mut raw[got..]) {
                    Ok(0) => break,
                    Ok(k) => got += k,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            if got == 0 || got < 4 * n {
                if !self.stopped() {
                    self.fail(format!("parec ended ({desc})"), None);
                }
                break;
            }
            let got = got - got % (4 * n);
            let frames = got / (4 * n);
            let mut planar = vec![Vec::with_capacity(frames); n];
            for (i, b) in raw[..got].chunks_exact(4).enumerate() {
                planar[i % n].push(f32::from_le_bytes([b[0], b[1], b[2], b[3]]));
            }
            let mut s = lock(&self.st);
            s.write_planar(self.slot, &planar);
        }
        *lock(&self.child_pid) = None;
        let _ = child.kill();
        let _ = child.wait();
        Ok(())
    }

    // -- MXL --
    fn run_mxl(&self) -> Result<(), PyErr> {
        let mxl = wbs_plugin::services::mxl();
        let flow = mxl
            .flow_info(&self.reference)
            .ok_or_else(|| PyErr::lookup(format!("MXL flow {} is not in /dev/shm/mxl", self.reference)))?;
        if flow.buffer_length == 0 || flow.sample_rate == 0 {
            return Err(PyErr { class: "ZeroDivisionError", msg: "integer modulo by zero".into() });
        }
        let mut rd = MxlReader::new(&self.reference, Instant::now());
        while !self.stopped() {
            let st = rd.poll(mxl, Instant::now());
            self.apply_mxl(&st);
            std::thread::sleep(rd.interval());
        }
        Ok(())
    }

    /// One MXL poll's outcome into the slot: geometry, status, audio, lost packets.
    fn apply_mxl(&self, st: &MxlStep) {
        if st.flow.is_none() && st.status.is_none() && st.audio.is_none() && st.lost == 0 {
            return;
        }
        let mut s = lock(&self.st);
        if let Some(f) = &st.flow {
            s.sample_rate = f.sample_rate;
            s.channels = f.channel_count;
            s.packet_frames = f.quantum;
            s.source = format!("{} \u{b7} {} (shared memory)", f.flow_id, f.domain_id);
            let same = s.ring.as_ref().is_some_and(|r| {
                (r.channels, r.packet_frames, r.sample_rate) == (f.channel_count, f.quantum, f.sample_rate)
            });
            if !same {
                s.ring = Some(TapRing::new(f.channel_count, f.quantum, f.sample_rate, RING_SECONDS));
                s.fold = None;
            }
        }
        if let Some((status, hint)) = &st.status {
            s.status = status.clone();
            s.hint = hint.clone();
            s.error = None;
        }
        s.lost += st.lost;
        if let Some(planar) = &st.audio {
            s.write_planar(self.slot, planar);
        }
    }

    // -- AES67 / RAVENNA RTP --
    fn resolve_sdp(&self) -> Option<Map<String, Value>> {
        if let Some(m) = manual_stream(&self.reference) {
            return Some(m);
        }
        let snap = wbs_plugin::services::aoip().snapshot();
        let sessions = snap.get("sap").and_then(|s| s.get("sessions")).and_then(|s| s.as_array()).cloned().unwrap_or_default();
        let r = json!(self.reference);
        for s in &sessions {
            if s.get("id") == Some(&r) || s.get("session_name") == Some(&r) {
                return s.as_object().cloned();
            }
        }
        // a device (RAVENNA / NMOS / Dante IP): its first announced stream
        for s in &sessions {
            if s.get("origin_ip") == Some(&r) || s.get("source_ip") == Some(&r) {
                return s.as_object().cloned();
            }
        }
        None
    }

    fn run_rtp(&self) -> Result<(), PyErr> {
        let mut s = self.resolve_sdp();
        let deadline = now_f64() + 40.0; // SAP re-announces every ~30 s: wait for one before giving up
        while s.is_none() && now_f64() < deadline && !self.stopped() {
            {
                let mut st = lock(&self.st);
                st.status = "waiting".into();
                st.hint = Some("waiting for a SAP announcement of this stream".into());
            }
            py::wait(&self.stop, 1.0);
            s = self.resolve_sdp();
        }
        lock(&self.st).hint = None;
        let nil = Value::Null;
        let get = |k: &str| s.as_ref().and_then(|m| m.get(k)).unwrap_or(&nil);
        if s.is_none() || !py::truthy(get("multicast_ip")) || !py::truthy(get("port")) {
            return Err(PyErr::lookup(format!(
                "no SDP heard for {}: Dante-native audio cannot be received (enable AES67 on the device) \
                 and RAVENNA/NMOS streams need a SAP announcement",
                self.label
            )));
        }
        let enc_v = get("encoding");
        let enc = if py::truthy(enc_v) { py::str_of(enc_v).to_uppercase() } else { "L24".into() };
        if enc != "L24" && enc != "L16" {
            return Err(PyErr::lookup(format!("{enc} RTP payload is not decoded (L24 / L16 only)")));
        }
        let bps = if enc == "L24" { 3 } else { 2 };
        let or_int = |k: &str, d: i64| -> Result<i64, PyErr> {
            let v = get(k);
            if py::truthy(v) { Ok(py::int_of(v)?) } else { Ok(d) }
        };
        let channels = or_int("channels", 2)?.clamp(i64::MIN, MAX_CHANNELS as i64).max(1) as usize;
        let sample_rate = or_int("sample_rate", 48000)? as u32;
        let grp = py::str_of(get("multicast_ip"));
        let port = py::int_of(get("port"))?;
        let ptime_v = get("ptime");
        let ptime = if py::truthy(ptime_v) { ptime_v.as_f64().unwrap_or(1.0) } else { 1.0 };
        let ptime_txt = if py::truthy(ptime_v) { py::str_of(ptime_v) } else { "1.0".into() };
        let source = format!(
            "{} \u{b7} {}:{} \u{b7} {}ch {} @ {}k \u{b7} ptime {} ms",
            py::str_of(get("session_name")),
            grp,
            port,
            channels,
            enc,
            py::fmt_g(sample_rate as f64 / 1000.0),
            ptime_txt
        );
        let packet_frames = ((sample_rate as f64 * ptime / 1000.0).round_ties_even() as i64).max(1) as usize;
        {
            let mut st = lock(&self.st);
            st.channels = channels;
            st.sample_rate = sample_rate;
            st.source = source;
            st.packet_frames = packet_frames;
            st.group = Some(grp.clone());
            st.ring = Some(TapRing::new(channels, packet_frames, sample_rate, RING_SECONDS));
        }

        use socket2::{Domain, InterfaceIndexOrAddress, Protocol, SockAddr, Socket, Type};
        let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        sock.set_reuse_address(true)?;
        wbs_sys::reuse_port(&sock);
        sock.set_recv_buffer_size(1 << 20)?;
        let iface = route_iface(&grp);
        let (mc0, t0) = (mcast_count(&iface), now_f64()); // NIC multicast rate before the join
        std::thread::sleep(Duration::from_secs(1));
        let base_rate = if mc0 >= 0 { (mcast_count(&iface) - mc0) as f64 / (now_f64() - t0).max(0.1) } else { -1.0 };
        let (mc1, t1) = (mcast_count(&iface), now_f64());
        let gaddr: Ipv4Addr = grp
            .parse()
            .map_err(|_| PyErr { class: "OSError", msg: "illegal IP address string passed to inet_aton".into() })?;
        let port_u16 = u16::try_from(port)
            .map_err(|_| PyErr { class: "OverflowError", msg: "bind(): port must be 0-65535.".into() })?;
        // bound to the group: only this stream, not every group on :port
        sock.bind(&SockAddr::from(SocketAddr::from((gaddr, port_u16))))?;
        let mut joined = 0;
        for (idx, _name) in py::if_nameindex() {
            if sock.join_multicast_v4_n(&gaddr, &InterfaceIndexOrAddress::Index(idx)).is_ok() {
                joined += 1;
            }
        }
        if joined == 0 {
            sock.join_multicast_v4(&gaddr, &Ipv4Addr::UNSPECIFIED)?;
        }
        sock.set_read_timeout(Some(Duration::from_millis(500)))?;
        let sock: UdpSocket = sock.into();
        lock(&self.st).status = "waiting".into();
        let n = channels;
        let scale = if bps == 3 { 1.0 / 2147483648.0 } else { 1.0 / 32768.0 };
        let mut last_seq: Option<u16> = None;
        let mut sized = false;
        let mut buf = vec![0u8; 9000];
        while !self.stopped() {
            let len = match sock.recv(&mut buf) {
                Ok(k) => k,
                Err(e) if py::is_timeout(&e) => {
                    let (packets, has_fix) = {
                        let st = lock(&self.st);
                        (st.ring.as_ref().map(|r| r.packets).unwrap_or(0), !st.fix.is_null())
                    };
                    if packets == 0 && now_f64() - t1 > 2.0 && !has_fix {
                        self.diagnose_rtp(&grp, port, &iface, base_rate, mc1, t1);
                    }
                    continue;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            let pkt = &buf[..len];
            let Some((payload, seq)) = rtp_payload(pkt) else { continue };
            let frames = payload.len() / (bps * n);
            if frames == 0 {
                continue;
            }
            if let Some(ls) = last_seq {
                let gap = seq.wrapping_sub(ls).wrapping_sub(1);
                if gap != 0 && gap < 1000 {
                    lock(&self.st).lost += gap as u64;
                }
            }
            last_seq = Some(seq);
            let planar = decode_pcm(&payload[..frames * bps * n], bps, n, scale);
            let mut st = lock(&self.st);
            if !sized && frames != st.packet_frames {
                // measured packet beats SDP ptime
                st.packet_frames = frames;
                st.ring = Some(TapRing::new(n, frames, sample_rate, RING_SECONDS));
            }
            sized = true;
            st.write_planar(self.slot, &planar);
            st.status = "running".into();
            st.hint = None;
            st.fix = Value::Null;
        }
        Ok(())
    }

    /// Joined but silent: tell the user why, and the one command that fixes it if there is one.
    fn diagnose_rtp(&self, grp: &str, port: i64, iface: &str, base_rate: f64, mc1: i64, t1: f64) {
        let rate = if mc1 >= 0 && base_rate >= 0.0 { (mcast_count(iface) - mc1) as f64 / (now_f64() - t1).max(0.1) } else { -1.0 };
        let (hint, fix) = diagnosis(grp, port, iface, base_rate, rate, ufw_enabled());
        let mut st = lock(&self.st);
        st.hint = Some(hint);
        st.fix = fix;
    }

    // -- views --
    fn info(&self) -> Value {
        let s = lock(&self.st);
        self.info_locked(&s)
    }

    fn info_locked(&self, s: &TpState) -> Value {
        let sr = s.sample_rate as f64;
        let mut m = Map::new();
        m.insert("kind".into(), json!(self.kind));
        m.insert("ref".into(), json!(self.reference));
        m.insert("label".into(), json!(self.label));
        m.insert("source".into(), json!(s.source));
        m.insert("status".into(), json!(s.status));
        m.insert("error".into(), json!(s.error));
        m.insert("hint".into(), json!(s.hint));
        m.insert("fix".into(), s.fix.clone());
        m.insert("sample_rate".into(), json!(s.sample_rate));
        m.insert("channel_count".into(), json!(s.channels));
        m.insert("packet_frames".into(), json!(s.packet_frames));
        m.insert(
            "packet_ms".into(),
            if s.packet_frames != 0 { json!(py::round(1000.0 * s.packet_frames as f64 / sr, 3)) } else { Value::Null },
        );
        m.insert("ring_frames".into(), json!(s.ring.as_ref().map(|r| r.length).unwrap_or(0)));
        m.insert(
            "ring_ms".into(),
            s.ring.as_ref().map(|r| json!(py::round(1000.0 * r.length as f64 / sr, 3))).unwrap_or(Value::Null),
        );
        m.insert("packets".into(), json!(s.ring.as_ref().map(|r| r.packets).unwrap_or(0)));
        m.insert("lost_packets".into(), json!(s.lost));
        Value::Object(m)
    }

    fn ring_snapshot(&self, bins: usize) -> Option<Value> {
        self.snapshot_of(bins, false)
    }

    /// The AMS-81 fold-down's ring, as its own stereo flow.
    fn fold_snapshot(&self, bins: usize) -> Option<Value> {
        self.snapshot_of(bins, true)
    }

    fn snapshot_of(&self, bins: usize, fold: bool) -> Option<Value> {
        let select = if fold { ams81::settings().get(self.slot)?.select_name() } else { String::new() };
        let s = lock(&self.st);
        let r = if fold { s.fold.as_ref()? } else { s.ring.as_ref()? };
        let chans = r.snapshot(bins);
        let head = r.head as i64;
        let mut d = match self.info_locked(&s) {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        let slot = &SLOTS[self.slot];
        if fold {
            d.insert("channel_count".into(), json!(r.channels));
            d.insert("flow_id".into(), json!(slot.fold_flow_id));
            d.insert("label".into(), json!(format!("{} \u{b7} AMS-81 {} \u{b7} {}", slot.title, select, self.label)));
        } else {
            d.insert("flow_id".into(), json!(slot.flow_id));
            d.insert("label".into(), json!(format!("{} \u{b7} {}", slot.title, self.label)));
        }
        d.insert("kind".into(), json!(slot.kind));
        d.insert("slot".into(), json!(slot.name));
        d.insert("buffer_length".into(), json!(r.length));
        d.insert("write_head".into(), json!(head));
        // meters read one packet behind the writer
        let meter_head = if r.length > 0 { (head - r.packet_frames as i64).rem_euclid(r.length as i64) } else { 0 };
        d.insert("meter_head".into(), json!(meter_head));
        d.insert("write_seq".into(), json!(r.packets));
        d.insert("channels".into(), Value::Array(chans));
        Some(Value::Object(d))
    }
}

/// A stream entered by hand: a pasted SDP ("v=0 ..."), or "239.1.2.3:5004" with optional
/// "/8ch/L24/48000/1.0" (channels, encoding, rate, ptime; defaults 2 ch L24 48 kHz 1 ms).
pub fn manual_stream(reference: &str) -> Option<Map<String, Value>> {
    let r = reference.trim();
    if r.starts_with("v=") {
        let mut m = wbs_plugin::services::aoip().parse_sdp(r).as_object().cloned()?;
        if !py::truthy(m.get("session_name").unwrap_or(&Value::Null)) {
            m.insert("session_name".into(), json!("custom SDP"));
        }
        return Some(m);
    }
    let mut parts = r.split('/');
    let (ip, port) = parts.next()?.rsplit_once(':')?;
    let ip: std::net::Ipv4Addr = ip.trim().parse().ok()?;
    let port: u16 = port.trim().parse().ok()?;
    let mut m = Map::new();
    m.insert("session_name".into(), json!(format!("{ip}:{port}")));
    m.insert("multicast_ip".into(), json!(ip.to_string()));
    m.insert("port".into(), json!(port));
    m.insert("channels".into(), json!(2));
    m.insert("encoding".into(), json!("L24"));
    m.insert("sample_rate".into(), json!(48000));
    m.insert("ptime".into(), json!(1.0));
    for p in parts.map(str::trim).filter(|p| !p.is_empty()) {
        let lower = p.to_ascii_lowercase();
        if let Some(c) = lower.strip_suffix("ch").and_then(|c| c.parse::<i64>().ok()) {
            m.insert("channels".into(), json!(c));
        } else if lower == "l24" || lower == "l16" {
            m.insert("encoding".into(), json!(p.to_ascii_uppercase()));
        } else if let Ok(n) = p.parse::<f64>() {
            if n >= 8000.0 { m.insert("sample_rate".into(), json!(n as i64)); } else { m.insert("ptime".into(), json!(n)); }
        }
    }
    Some(m)
}

/// (parec device args, description, channel count) of a tappable PipeWire node.
pub type PwTarget = (Vec<String>, String, i64);

/// Resolve a hw (ALSA card) or pw (PipeWire node / port id) reference against a pw-dump
/// object list -> ((parec device args, description, channels), objs).
pub fn resolve_pipewire_in(kind: &str, reference: &str, objs: Vec<Value>) -> Result<(PwTarget, Vec<Value>), PyErr> {
    let empty = Map::new();
    let props = |o: &Value| -> Map<String, Value> {
        o.get("info").and_then(|i| i.get("props")).and_then(|p| p.as_object()).unwrap_or(&empty).clone()
    };
    let s = |m: &Map<String, Value>, k: &str| -> String { m.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string() };
    let typ = |o: &Value| -> String { o.get("type").and_then(|t| t.as_str()).unwrap_or("").to_string() };
    let mut by_id: HashMap<Option<i64>, &Value> = HashMap::new();
    for o in &objs {
        by_id.insert(o.get("id").and_then(|v| v.as_i64()), o);
    }

    fn node_target(
        nid: Option<i64>,
        depth: u32,
        objs: &[Value],
        by_id: &HashMap<Option<i64>, &Value>,
        props: &dyn Fn(&Value) -> Map<String, Value>,
    ) -> Result<Option<PwTarget>, PyErr> {
        let o = match by_id.get(&nid) {
            Some(o) => *o,
            None => return Ok(None),
        };
        if !o.get("type").and_then(|t| t.as_str()).unwrap_or("").ends_with("Node") {
            return Ok(None);
        }
        let p = props(o);
        let gs = |k: &str| p.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let mc = gs("media.class");
        let name = gs("node.name");
        let chv = p.get("audio.channels").cloned().unwrap_or(Value::Null);
        let ch = if py::truthy(&chv) { py::int_of(&chv)? } else { 2 };
        if mc.starts_with("Audio/Source") {
            return Ok(Some((vec![format!("--device={name}")], format!("{name} (source)"), ch)));
        }
        if mc.starts_with("Audio/Sink") {
            return Ok(Some((vec![format!("--device={name}.monitor")], format!("{name} (sink monitor)"), ch)));
        }
        if mc == "Stream/Output/Audio" {
            let serial = py::str_of(p.get("object.serial").unwrap_or(&Value::Null));
            let app = p.get("application.name").filter(|v| py::truthy(v)).map(py::str_of).unwrap_or(name.clone());
            return Ok(Some((vec![format!("--monitor-stream={serial}")], format!("{app} (playback stream)"), ch)));
        }
        if mc == "Stream/Input/Audio" && depth == 0 {
            for l in objs {
                if l.get("type").and_then(|t| t.as_str()).unwrap_or("").ends_with("Link") {
                    let li = l.get("info").cloned().unwrap_or(Value::Null);
                    if li.get("input-node-id").and_then(|v| v.as_i64()) == nid && nid.is_some() {
                        let up = li.get("output-node-id").and_then(|v| v.as_i64());
                        if let Some(t) = node_target(up, 1, objs, by_id, props)? {
                            let who = if name.is_empty() { "capture stream".to_string() } else { name.clone() };
                            return Ok(Some((t.0, format!("{} -> {}", t.1, who), t.2)));
                        }
                    }
                }
            }
            return Ok(None);
        }
        Ok(None)
    }

    if kind == "hw" {
        let card = py::strip(&reference.replace("card", "")).to_string();
        let mut cands: Vec<&Value> = objs
            .iter()
            .filter(|o| {
                let p = props(o);
                typ(o).ends_with("Node")
                    && py::str_of(p.get("alsa.card").unwrap_or(&json!(""))) == card
                    && (s(&p, "media.class").starts_with("Audio/Source") || s(&p, "media.class").starts_with("Audio/Sink"))
            })
            .collect();
        cands.sort_by_key(|o| if s(&props(o), "media.class").starts_with("Audio/Source") { 0 } else { 1 });
        for o in cands {
            if let Some(t) = node_target(o.get("id").and_then(|v| v.as_i64()), 0, &objs, &by_id, &props)? {
                return Ok((t, objs.clone()));
            }
        }
        return Err(PyErr::lookup(format!("ALSA card {card} has no PipeWire node (profile off or card busy)")));
    }

    let mut oid = py::parse_int(reference.split('.').next().unwrap_or(""))
        .ok_or_else(|| PyErr::lookup(format!("not a PipeWire object id: {reference}")))?;
    let o = by_id.get(&Some(oid)).copied().ok_or_else(|| PyErr::lookup(format!("PipeWire object {oid} is gone")))?;
    if typ(o).ends_with("Port") {
        oid = py::int_of(props(o).get("node.id").unwrap_or(&json!(-1)))?;
    }
    match node_target(Some(oid), 0, &objs, &by_id, &props)? {
        Some(t) => Ok((t, objs.clone())),
        None => Err(PyErr::lookup(format!("PipeWire object {reference} carries no tappable audio"))),
    }
}

/// RTP header -> (payload, sequence number); None if not RTP v2 / too short.
pub fn rtp_payload(pkt: &[u8]) -> Option<(&[u8], u16)> {
    if pkt.len() < 12 || (pkt[0] >> 6) != 2 {
        return None;
    }
    let len = pkt.len() as i64;
    let cc = (pkt[0] & 0x0F) as i64;
    let mut off = 12 + 4 * cc;
    if pkt[0] & 0x10 != 0 && len >= off + 4 {
        let o = off as usize;
        off += 4 + 4 * u16::from_be_bytes([pkt[o + 2], pkt[o + 3]]) as i64;
    }
    let mut end = len - if pkt[0] & 0x20 != 0 { pkt[len as usize - 1] as i64 } else { 0 };
    // Python slice semantics: negative end counts from the back, everything clamps
    if end < 0 {
        end = (end + len).max(0);
    }
    let off = off.min(len);
    let payload: &[u8] = if off < end { &pkt[off as usize..end as usize] } else { &[] };
    Some((payload, u16::from_be_bytes([pkt[2], pkt[3]])))
}

/// Big-endian L24 / L16 interleaved PCM -> planar f32.
pub fn decode_pcm(payload: &[u8], bps: usize, channels: usize, scale: f64) -> Vec<Vec<f32>> {
    let frames = payload.len() / (bps * channels);
    let mut planar = vec![Vec::with_capacity(frames); channels];
    for (i, s) in payload.chunks_exact(bps).enumerate() {
        let v = if bps == 3 {
            i32::from_be_bytes([s[0], s[1], s[2], 0]) as f64
        } else {
            i16::from_be_bytes([s[0], s[1]]) as f64
        };
        planar[i % channels].push((v * scale) as f32);
    }
    planar
}

/// The silent-RTP diagnosis: (hint, fix) from the NIC multicast rates and UFW state.
pub fn diagnosis(grp: &str, port: i64, iface: &str, base_rate: f64, rate: f64, ufw: bool) -> (String, Value) {
    let arriving = rate >= 0.0 && rate - base_rate > 50.0;
    let nic = if arriving {
        format!(
            "{iface} is receiving {:.0} multicast frames/s since the join (was {:.0}/s), so the stream reaches this machine",
            rate, base_rate
        )
    } else if rate >= 0.0 {
        format!("{iface} shows no extra multicast traffic since the join")
    } else {
        String::new()
    };
    if ufw {
        let i_opt = if iface.is_empty() { String::new() } else { format!(" -i {iface}") };
        let on = if iface.is_empty() { "any" } else { iface };
        let fix = json!({
            "title": "Allow AES67 / RAVENNA audio through the firewall",
            "why": format!("{nic}, but UFW (incoming policy DROP) discards it before the socket. \
                            Opening it needs your password, so run one of these in a terminal."),
            "options": [
                {"label": "This session only", "note": "the rule is gone at the next reboot",
                 "command": format!("sudo iptables -I INPUT{i_opt} -p udp -d 239.0.0.0/8 --dport {port} -j ACCEPT")},
                {"label": "Permanently", "note": "saved as a UFW rule",
                 "command": format!("sudo ufw allow in on {on} proto udp to 239.0.0.0/8 port {port}")},
            ],
        });
        (format!("{grp}:{port} is blocked by the UFW firewall"), fix)
    } else if arriving {
        let fix = json!({
            "title": "Check the host firewall",
            "why": format!("{nic}, but nothing reaches the socket; a firewall rule is dropping it. List the rules:"),
            "options": [{"label": "List firewall rules", "note": "read-only", "command": "sudo nft list ruleset"}],
        });
        (format!("{grp}:{port} reaches {iface} but not the socket"), fix)
    } else {
        let on = if iface.is_empty() { "any interface" } else { iface };
        (
            format!("no RTP for {grp}:{port} on {on}: the sender may be stopped, or the switch is not forwarding this group (IGMP)"),
            Value::Null,
        )
    }
}

// ---------------------------------------------------------------------------
// API
// ---------------------------------------------------------------------------

fn active() -> &'static Mutex<[Option<Shared>; 3]> {
    static A: OnceLock<Mutex<[Option<Shared>; 3]>> = OnceLock::new();
    A.get_or_init(|| Mutex::new([None, None, None]))
}

fn current_in(slot: usize) -> Option<Shared> {
    lock(active()).get(slot).cloned().flatten()
}

pub fn set_in(slot: usize, kind: &str, reference: &str, label: &str) -> Value {
    stop_in(slot);
    let tp = TestPoint::new(slot, kind, reference, label);
    wbs_log::info(SLOTS[slot].name, &format!("{} {} ({})", kind, reference, tp.label));
    lock(active())[slot] = Some(tp.clone());
    tp.start();
    std::thread::sleep(Duration::from_millis(150));
    tp.info()
}

pub fn stop_in(slot: usize) -> bool {
    let tp = lock(active())[slot].take();
    match tp {
        Some(tp) => {
            wbs_log::info(SLOTS[slot].name, &format!("stopped ({})", tp.label));
            tp.stop();
            true
        }
        None => false,
    }
}

pub fn status_in(slot: usize) -> Value {
    let tp = current_in(slot);
    let mut m = Map::new();
    m.insert("active".into(), json!(tp.is_some()));
    m.insert("flow_id".into(), json!(SLOTS[slot].flow_id));
    if !SLOTS[slot].fold_flow_id.is_empty() {
        m.insert("fold_flow_id".into(), json!(SLOTS[slot].fold_flow_id));
        m.insert("ams81".into(), json!(ams81::settings()[slot].select_name()));
    }
    m.insert("point".into(), tp.map(|t| t.info()).unwrap_or(Value::Null));
    Value::Object(m)
}

pub fn ring_snapshot_in(slot: usize, bins: usize) -> Option<Value> {
    current_in(slot)?.ring_snapshot(bins)
}

pub fn pcm_since_in(slot: usize, since: i64, max_frames: usize) -> Option<PcmChunk> {
    let tp = current_in(slot)?;
    let s = lock(&tp.st);
    s.ring.as_ref()?.pcm_since(since, max_frames)
}

/// The multicast group slot `slot` is receiving (None: not receiving RTP).
pub fn source_group_in(slot: usize) -> Option<String> {
    let tp = current_in(slot)?;
    let s = lock(&tp.st);
    s.group.clone()
}

pub fn fold_snapshot_in(slot: usize, bins: usize) -> Option<Value> {
    current_in(slot)?.fold_snapshot(bins)
}

/// New audio of a receiver's AMS-81 fold-down (same cursor semantics as pcm_since_in).
pub fn fold_pcm_since_in(slot: usize, since: i64, max_frames: usize) -> Option<PcmChunk> {
    let tp = current_in(slot)?;
    let s = lock(&tp.st);
    s.fold.as_ref()?.pcm_since(since, max_frames)
}

/// Every receiver's fold-down: {network: {select, presets, flow_id}, moip: {...}}.
pub fn ams81_status() -> Value {
    let a = ams81::settings();
    let mut m = Map::new();
    for (i, s) in SLOTS.iter().enumerate().filter(|(_, s)| !s.fold_flow_id.is_empty()) {
        let mut v = a[i].to_json();
        if let Some(o) = v.as_object_mut() {
            o.insert("flow_id".into(), json!(s.fold_flow_id));
        }
        m.insert(s.name.into(), v);
    }
    Value::Object(m)
}

/// The router's dedicated receivers: {network: {active, flow_id, point}, moip: {...}}.
pub fn receivers_status() -> Value {
    let mut m = Map::new();
    for (i, s) in SLOTS.iter().enumerate().skip(1) {
        m.insert(s.name.into(), status_in(i));
    }
    Value::Object(m)
}

pub fn stop_receivers() -> bool {
    (1..SLOTS.len()).fold(false, |any, i| stop_in(i) || any)
}

pub fn set_point(kind: &str, reference: &str, label: &str) -> Value {
    set_in(TEST_POINT, kind, reference, label)
}

pub fn stop_point() -> bool {
    stop_in(TEST_POINT)
}

pub fn status() -> Value {
    let mut v = status_in(TEST_POINT);
    if let Some(o) = v.as_object_mut() {
        o.remove("flow_id");
    }
    v
}

pub fn ring_snapshot(bins: usize) -> Option<Value> {
    ring_snapshot_in(TEST_POINT, bins)
}

/// New ring audio since the since cursor (ring position; < 0 = start now, no frames), newest
/// max_frames at most. Same semantics as the /pcm route in ward-beck-audio-tab.py.
pub fn pcm_since(since: i64, max_frames: usize) -> Option<PcmChunk> {
    pcm_since_in(TEST_POINT, since, max_frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_streams() {
        let m = manual_stream("239.1.44.167:5004/8ch/L24/48000/0.125").unwrap();
        assert_eq!((m["multicast_ip"].as_str(), m["port"].as_i64(), m["channels"].as_i64()), (Some("239.1.44.167"), Some(5004), Some(8)));
        assert_eq!(m["ptime"].as_f64(), Some(0.125));
        assert_eq!(manual_stream("239.3.44.167:5004").unwrap()["channels"].as_i64(), Some(2));
        assert!(manual_stream("Desk preMO").is_none());
    }

    fn tone(n: usize, amp: f32, ch: usize) -> Vec<Vec<f32>> {
        (0..ch).map(|c| (0..n).map(|i| amp * ((i as f32 * 0.05 + c as f32).sin())).collect()).collect()
    }

    #[test]
    fn ring_geometry() {
        let r = TapRing::new(2, 1024, 48000, 0.0);
        assert_eq!(r.length, 10240);
        assert_eq!(r.nb, 720);
        let r = TapRing::new(8, 48, 48000, RING_SECONDS);
        assert_eq!(r.length, 480000);
        let r = TapRing::new(1, 32, 48000, 0.0);
        assert_eq!(r.nb, 320);
    }

    #[test]
    fn ring_write_wraps_and_meters() {
        let mut r = TapRing::new(2, 1024, 48000, 0.0);
        for _ in 0..12 {
            r.write(&tone(1024, 0.5, 2));
        }
        assert_eq!(r.packets, 12);
        assert_eq!(r.head, (12 * 1024) % 10240);
        assert!(r.peak[0] > 0.49 && r.peak[0] <= 0.5);
        let snap = r.snapshot(60);
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0]["min"].as_array().unwrap().len(), 60);
        assert!(snap[0]["peak_db"].as_f64().unwrap() < -6.0);
        let keys: Vec<&String> = snap[0].as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["min", "max", "peak_db", "rms_db"]);
        // oversize write keeps the newest `length` frames
        let mut big = vec![vec![0.0f32; 20000]; 2];
        big[0][19999] = 0.25;
        r.write(&big);
        assert_eq!(r.data[0][(r.head + r.length - 1) % r.length], 0.25);
    }

    #[test]
    fn pcm_since_semantics() {
        let mut r = TapRing::new(1, 100, 48000, 0.0); // 1000 frames
        let p: Vec<f32> = (0..300).map(|i| i as f32).collect();
        r.write(&[p]);
        let c = r.pcm_since(-1, 9600).unwrap();
        assert_eq!((c.cursor, c.frames, c.payload.len()), (300, 0, 0));
        let c = r.pcm_since(100, 9600).unwrap();
        assert_eq!((c.cursor, c.frames, c.payload.len()), (300, 200, 800));
        assert_eq!(f32::from_le_bytes(c.payload[0..4].try_into().unwrap()), 100.0);
        let c = r.pcm_since(0, 50).unwrap(); // fell behind: newest window
        assert_eq!(c.frames, 50);
        assert_eq!(f32::from_le_bytes(c.payload[0..4].try_into().unwrap()), 250.0);
        // wrap
        let p: Vec<f32> = (0..800).map(|i| 1000.0 + i as f32).collect();
        r.write(&[p]);
        let c = r.pcm_since(900, 9600).unwrap();
        assert_eq!((c.cursor, c.frames), (100, 200));
        assert_eq!(f32::from_le_bytes(c.payload[0..4].try_into().unwrap()), 1600.0);
    }

    #[test]
    fn graph_quantum_parse() {
        let out = "update: id:0 key:'clock.rate' value:'48000' type:''\n\
                   update: id:0 key:'clock.quantum' value:'1024' type:''\n\
                   update: id:0 key:'clock.force-quantum' value:'0' type:''\n";
        assert_eq!(parse_graph_quantum(out), 1024);
        assert_eq!(parse_graph_quantum(&out.replace("value:'0'", "value:'256'")), 256);
        assert_eq!(parse_graph_quantum(""), 1024);
        assert_eq!(parse_graph_quantum("key:'clock.quantum' value:'8'"), 32);
    }

    #[test]
    fn rtp_decode() {
        let mut pkt = vec![0x80, 96, 0x00, 0x05, 0, 0, 0, 0, 0, 0, 0, 0];
        // 2 frames x 2 ch L24: +half, -half, 0, max
        pkt.extend_from_slice(&[0x40, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0x7F, 0xFF, 0xFF]);
        let (pl, seq) = rtp_payload(&pkt).unwrap();
        assert_eq!(seq, 5);
        let p = decode_pcm(pl, 3, 2, 1.0 / 2147483648.0);
        assert_eq!(p[0], vec![0.5, 0.0]);
        assert_eq!(p[1][0], -0.5);
        assert!(p[1][1] > 0.9999);
        // padding + extension
        let mut x = vec![0x80 | 0x20 | 0x10, 96, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0xBE, 0xDE, 0, 1, 1, 2, 3, 4];
        x.extend_from_slice(&[0x12, 0x34, 0, 0, 2]);
        let (pl, _) = rtp_payload(&x).unwrap();
        assert_eq!(pl, &[0x12, 0x34, 0]);
        let p = decode_pcm(&[0x80, 0x00], 2, 1, 1.0 / 32768.0);
        assert_eq!(p[0], vec![-1.0]);
        assert!(rtp_payload(&[0u8; 12]).is_none());
    }

    #[test]
    fn diagnosis_texts() {
        let (h, f) = diagnosis("239.1.44.158", 5004, "enp5s0f0", 10.0, 1010.0, true);
        assert_eq!(h, "239.1.44.158:5004 is blocked by the UFW firewall");
        assert_eq!(
            f["why"],
            "enp5s0f0 is receiving 1010 multicast frames/s since the join (was 10/s), so the stream reaches this machine, \
             but UFW (incoming policy DROP) discards it before the socket. Opening it needs your password, so run one of these in a terminal."
        );
        assert_eq!(f["options"][0]["command"], "sudo iptables -I INPUT -i enp5s0f0 -p udp -d 239.0.0.0/8 --dport 5004 -j ACCEPT");
        assert_eq!(f["options"][1]["command"], "sudo ufw allow in on enp5s0f0 proto udp to 239.0.0.0/8 port 5004");
        let (h, f) = diagnosis("239.1.44.158", 5004, "eth0", 0.0, 500.0, false);
        assert_eq!(h, "239.1.44.158:5004 reaches eth0 but not the socket");
        assert_eq!(f["title"], "Check the host firewall");
        let (h, f) = diagnosis("239.1.44.158", 5004, "", -1.0, -1.0, false);
        assert_eq!(h, "no RTP for 239.1.44.158:5004 on any interface: the sender may be stopped, or the switch is not forwarding this group (IGMP)");
        assert!(f.is_null());
    }

    #[test]
    fn pipewire_resolution() {
        let objs = json!([
            {"id": 50, "type": "PipeWire:Interface:Node", "info": {"props": {"media.class": "Audio/Sink", "node.name": "wbs_meter_tap", "audio.channels": 2}}},
            {"id": 60, "type": "PipeWire:Interface:Node", "info": {"props": {"media.class": "Stream/Input/Audio", "node.name": "parec"}}},
            {"id": 61, "type": "PipeWire:Interface:Port", "info": {"props": {"node.id": 60}}},
            {"id": 70, "type": "PipeWire:Interface:Link", "info": {"input-node-id": 60, "output-node-id": 50}},
            {"id": 80, "type": "PipeWire:Interface:Node", "info": {"props": {"media.class": "Audio/Source", "node.name": "alsa_input.x", "alsa.card": 1, "audio.channels": 4}}},
            {"id": 81, "type": "PipeWire:Interface:Node", "info": {"props": {"media.class": "Stream/Output/Audio", "node.name": "n", "object.serial": 1234, "application.name": "Firefox"}}}
        ]);
        let objs = objs.as_array().unwrap().clone();
        let ((a, d, c), _) = resolve_pipewire_in("pw", "61", objs.clone()).unwrap();
        assert_eq!(a, vec!["--device=wbs_meter_tap.monitor"]);
        assert_eq!(d, "wbs_meter_tap (sink monitor) -> parec");
        assert_eq!(c, 2);
        let ((a, d, c), _) = resolve_pipewire_in("hw", "card1", objs.clone()).unwrap();
        assert_eq!((a[0].as_str(), d.as_str(), c), ("--device=alsa_input.x", "alsa_input.x (source)", 4));
        let ((a, d, _), _) = resolve_pipewire_in("pw", "81", objs.clone()).unwrap();
        assert_eq!((a[0].as_str(), d.as_str()), ("--monitor-stream=1234", "Firefox (playback stream)"));
        let e = resolve_pipewire_in("pw", "999", objs.clone()).unwrap_err();
        assert_eq!(format!("{}: {}", e.class, e.msg), "LookupError: PipeWire object 999 is gone");
        let e = resolve_pipewire_in("pw", "abc", objs.clone()).unwrap_err();
        assert_eq!(e.msg, "not a PipeWire object id: abc");
        let e = resolve_pipewire_in("hw", "card7", objs.clone()).unwrap_err();
        assert_eq!(e.msg, "ALSA card 7 has no PipeWire node (profile off or card busy)");
        let e = resolve_pipewire_in("pw", "70", objs).unwrap_err();
        assert_eq!(e.msg, "PipeWire object 70 carries no tappable audio");
    }

    #[test]
    fn info_contract_and_unknown_kind() {
        let tp = TestPoint::new(TEST_POINT, "nmos", "x", "");
        tp.start();
        std::thread::sleep(Duration::from_millis(50));
        let i = tp.info();
        let keys: Vec<&String> = i.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec![
                "kind", "ref", "label", "source", "status", "error", "hint", "fix", "sample_rate", "channel_count",
                "packet_frames", "packet_ms", "ring_frames", "ring_ms", "packets", "lost_packets"
            ]
        );
        assert_eq!(i["status"], "error");
        assert_eq!(i["error"], "'nmos' points cannot be metered");
        assert_eq!(i["label"], "x");
        assert!(i["packet_ms"].is_null() && i["ring_ms"].is_null());
        tp.stop();
    }

    #[test]
    fn receivers_are_their_own_flows() {
        assert_eq!(slot_named("network"), Some(1));
        assert_eq!(slot_named("moip"), Some(2));
        assert_eq!(slot_of_flow("wbs-moip-rx"), Some(2));
        let tp = TestPoint::new(1, "aoip", "studio", "");
        lock(&tp.st).ring = Some(TapRing::new(2, 48, 48000, 0.0));
        let d = tp.ring_snapshot(60).unwrap();
        assert_eq!(d["flow_id"], "wbs-network-rx");
        assert_eq!(d["kind"], "receiver");
        assert_eq!(d["slot"], "network");
        assert_eq!(d["label"], "NETWORK RX \u{b7} studio");
        let r = receivers_status();
        assert_eq!(r["moip"]["flow_id"], "wbs-moip-rx");
        assert_eq!(r["network"]["active"], false);
    }

    #[test]
    fn receivers_fold_to_ams81() {
        assert_eq!(slot_of_fold("wbs-network-ams81"), Some(1));
        assert_eq!(slot_of_fold("wbs-moip-ams81"), Some(2));
        assert_eq!(slot_of_fold(""), None);
        assert_eq!(slot_of_flow("wbs-moip-ams81"), None);
        let eight: Vec<Vec<f32>> = (0..8).map(|c| vec![(c + 1) as f32 / 10.0; 48]).collect();
        // the test point keeps no fold-down
        let t0 = TestPoint::new(TEST_POINT, "pw", "1", "");
        {
            let mut s = lock(&t0.st);
            s.ring = Some(TapRing::new(8, 48, 48000, 0.0));
            s.write_planar(TEST_POINT, &eight);
            assert!(s.fold.is_none() && s.ring.as_ref().unwrap().packets == 1);
        }
        // a receiver folds each packet into a stereo ring at the same packet size / rate
        let t = TestPoint::new(1, "aoip", "studio", "");
        let want = ams81::settings()[1].fold(&eight);
        {
            let mut s = lock(&t.st);
            s.channels = 8;
            s.write_planar(1, &eight); // no ring yet: nothing
            assert!(s.fold.is_none());
            s.ring = Some(TapRing::new(8, 48, 48000, RING_SECONDS));
            s.write_planar(1, &eight);
            let f = s.fold.as_ref().unwrap();
            assert_eq!((f.channels, f.packet_frames, f.sample_rate, f.packets), (2, 48, 48000, 1));
            assert_eq!(f.length, s.ring.as_ref().unwrap().length);
            assert_eq!((f.data[0][0], f.data[1][47]), (want[0][0], want[1][47]));
            // a new packet size rebuilds the fold ring
            s.ring = Some(TapRing::new(8, 96, 48000, RING_SECONDS));
            s.write_planar(1, &eight);
            assert_eq!((s.fold.as_ref().unwrap().packet_frames, s.fold.as_ref().unwrap().packets), (96, 1));
        }
        let d = t.fold_snapshot(60).unwrap();
        assert_eq!(d["flow_id"], "wbs-network-ams81");
        assert_eq!(d["channel_count"], 2);
        assert_eq!(d["channels"].as_array().unwrap().len(), 2);
        let sel = ams81::settings()[1].select_name();
        assert_eq!(d["label"], format!("NETWORK RX \u{b7} AMS-81 {sel} \u{b7} studio"));
        assert_eq!(t.ring_snapshot(60).unwrap()["channel_count"], 8);
        // status and the fold-down API
        let r = receivers_status();
        assert_eq!(r["network"]["fold_flow_id"], "wbs-network-ams81");
        assert_eq!(r["moip"]["ams81"], json!(ams81::settings()[2].select_name()));
        assert!(status().get("fold_flow_id").is_none());
        let a = ams81_status();
        assert_eq!(a["moip"]["flow_id"], "wbs-moip-ams81");
        assert_eq!(a["network"]["presets"]["D"]["R"].as_array().unwrap().len(), 8);
        assert!(a.get("testpoint").is_none());
    }

    #[test]
    fn ring_snapshot_contract() {
        let tp = TestPoint::new(TEST_POINT, "pw", "111", "lbl");
        {
            let mut s = lock(&tp.st);
            s.packet_frames = 1024;
            let mut r = TapRing::new(2, 1024, 48000, 0.0);
            for _ in 0..6 {
                r.write(&tone(1024, 0.0, 2));
            }
            s.ring = Some(r);
        }
        let d = tp.ring_snapshot(60).unwrap();
        let keys: Vec<&String> = d.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec![
                "kind", "ref", "label", "source", "status", "error", "hint", "fix", "sample_rate", "channel_count",
                "packet_frames", "packet_ms", "ring_frames", "ring_ms", "packets", "lost_packets", "flow_id",
                "slot", "buffer_length", "write_head", "meter_head", "write_seq", "channels"
            ]
        );
        assert_eq!(d["kind"], "testpoint");
        assert_eq!(d["label"], "TEST POINT \u{b7} lbl");
        assert_eq!(d["packet_ms"], 21.333);
        assert_eq!(d["ring_ms"], 213.333);
        assert_eq!((&d["write_head"], &d["meter_head"]), (&json!(6144), &json!(5120)));
        assert_eq!(d["channels"][0]["peak_db"], -120.0);
        assert_eq!(serde_json::to_string(&d["channels"][0]["min"][0]).unwrap(), "0.0");
    }

    /// A foreign MXL writer the tests start, stop, restart and resize.
    struct FakeFlow {
        st: Mutex<FakeState>,
    }
    struct FakeState {
        present: bool,
        has_index: bool,
        index: u64,
        info: FlowInfo,
    }
    impl FakeFlow {
        fn new(l: usize, ch: usize) -> Self {
            FakeFlow { st: Mutex::new(FakeState { present: true, has_index: true, index: 1_000_000, info: Self::info(l, ch) }) }
        }
        fn info(l: usize, ch: usize) -> FlowInfo {
            FlowInfo {
                flow_id: "cbc-player".into(),
                label: "CBC".into(),
                domain_id: "domain-wbs-test-signals".into(),
                channel_count: ch,
                sample_rate: 48000,
                quantum: 48,
                buffer_length: l,
            }
        }
        fn advance(&self, n: u64) {
            lock(&self.st).index += n;
        }
    }
    impl Mxl for FakeFlow {
        fn flow_info(&self, id: &str) -> Option<FlowInfo> {
            let s = lock(&self.st);
            (s.present && id == "cbc-player").then(|| s.info.clone())
        }
        fn write_head(&self, _id: &str) -> Option<usize> {
            let s = lock(&self.st);
            s.present.then(|| (s.index % s.info.buffer_length as u64) as usize)
        }
        fn write_index(&self, _id: &str) -> Option<u64> {
            let s = lock(&self.st);
            (s.present && s.has_index).then_some(s.index)
        }
        // each sample is its ring position, so a read shows where it came from
        fn read_planar(&self, _id: &str, start: usize, count: usize) -> Vec<Vec<f32>> {
            let s = lock(&self.st);
            let l = s.info.buffer_length;
            (0..s.info.channel_count).map(|_| (0..count).map(|k| ((start + k) % l) as f32).collect()).collect()
        }
    }

    fn ms(t: Instant, n: u64) -> Instant {
        t + Duration::from_millis(n)
    }

    #[test]
    fn mxl_reader_follows_writer_through_stop_and_restart() {
        let fake = FakeFlow::new(4800, 2);
        let t = Instant::now();
        let mut rd = MxlReader::new("cbc-player", t);
        // first poll: resolved, running, nothing stale read
        let s = rd.poll(&fake, t);
        assert_eq!(s.flow.as_ref().unwrap().buffer_length, 4800);
        assert_eq!(s.status, Some(("running".into(), None)));
        assert!(s.audio.is_none());
        let w0 = (1_000_000 % 4800) as f32;
        // the writer moves: exactly the new frames
        fake.advance(480);
        let s = rd.poll(&fake, ms(t, 10));
        let a = s.audio.unwrap();
        assert_eq!((a.len(), a[0].len(), a[0][0]), (2, 480, w0));
        assert!(s.status.is_none() && s.flow.is_none() && s.lost == 0);

        // flow dir removed: waiting, said once, slot kept on the id
        lock(&fake.st).present = false;
        let s = rd.poll(&fake, ms(t, 20));
        assert_eq!(s.status, Some(("waiting".into(), Some(MXL_GONE.into()))));
        assert!(rd.poll(&fake, ms(t, 70)).status.is_none());
        assert_eq!(rd.id, "cbc-player");

        // back with a new ring (length, channels) and a fresh absolute index
        {
            let mut f = lock(&fake.st);
            f.present = true;
            f.index = 100;
            f.info = FakeFlow::info(9600, 4);
        }
        let s = rd.poll(&fake, ms(t, 120));
        let f = s.flow.unwrap();
        assert_eq!((f.buffer_length, f.channel_count), (9600, 4));
        assert_eq!(s.status, Some(("running".into(), None)));
        assert!(s.audio.is_none());
        fake.advance(48);
        let a = rd.poll(&fake, ms(t, 121)).audio.unwrap();
        assert_eq!((a.len(), a[0].len(), a[0][0]), (4, 48, 100.0));

        // head stops: quiet under 1 s, then "writer stopped"
        assert!(rd.poll(&fake, ms(t, 700)).status.is_none());
        let s = rd.poll(&fake, ms(t, 1300));
        assert_eq!(s.status, Some(("waiting".into(), Some(MXL_STOPPED.into()))));
        assert_eq!(rd.stalls, 2);
        // it moves again: running from the new head, then audio as usual
        fake.advance(48);
        let s = rd.poll(&fake, ms(t, 1400));
        assert_eq!(s.status, Some(("running".into(), None)));
        assert!(s.audio.is_none());
        fake.advance(48);
        assert_eq!(rd.poll(&fake, ms(t, 1401)).audio.unwrap()[0].len(), 48);

        // restart in place (index back to 7): resync, no stale ring, nothing counted lost
        lock(&fake.st).index = 7;
        let s = rd.poll(&fake, ms(t, 1402));
        assert!(s.audio.is_none() && s.lost == 0 && s.status.is_none());
        assert_eq!(rd.restarts, 1);
        fake.advance(48);
        let a = rd.poll(&fake, ms(t, 1403)).audio.unwrap();
        assert_eq!((a[0].len(), a[0][0]), (48, 7.0));
    }

    #[test]
    fn mxl_reader_overrun_and_unindexed_jump() {
        let fake = FakeFlow::new(4800, 2);
        let t = Instant::now();
        let mut rd = MxlReader::new("cbc-player", t);
        rd.poll(&fake, t);
        // the reader was away 200 ms while the writer wrote 9600 frames into a 4800 ring:
        // an overrun of (9600 - 4800) / 48 packets, then on from the head
        fake.advance(9600);
        let s = rd.poll(&fake, ms(t, 200));
        assert!(s.audio.is_none());
        assert_eq!((s.lost, rd.restarts), (100, 0));
        fake.advance(48);
        assert_eq!(rd.poll(&fake, ms(t, 201)).audio.unwrap()[0].len(), 48);
        // a writer with no absolute index: a head far beyond the elapsed time is a restart
        lock(&fake.st).has_index = false;
        fake.advance(48);
        assert_eq!(rd.poll(&fake, ms(t, 202)).audio.unwrap()[0].len(), 48);
        fake.advance(3000);
        let s = rd.poll(&fake, ms(t, 203));
        assert!(s.audio.is_none() && s.lost == 0);
        assert_eq!(rd.restarts, 1);
        fake.advance(48);
        assert_eq!(rd.poll(&fake, ms(t, 204)).audio.unwrap()[0].len(), 48);
    }

    #[test]
    fn mxl_step_into_slot() {
        let tp = TestPoint::new(1, "mxl", "cbc-player", "");
        let fake = FakeFlow::new(4800, 2);
        let t = Instant::now();
        let mut rd = MxlReader::new("cbc-player", t);
        tp.apply_mxl(&rd.poll(&fake, t));
        fake.advance(480);
        tp.apply_mxl(&rd.poll(&fake, ms(t, 10)));
        let i = tp.info();
        assert_eq!((i["status"].as_str(), i["packets"].as_u64(), i["channel_count"].as_u64()), (Some("running"), Some(1), Some(2)));
        lock(&fake.st).present = false;
        tp.apply_mxl(&rd.poll(&fake, ms(t, 20)));
        let i = tp.info();
        assert_eq!((i["status"].as_str(), i["hint"].as_str()), (Some("waiting"), Some(MXL_GONE)));
        // back with 4 channels: the ring follows, the fold is rebuilt from it
        {
            let mut f = lock(&fake.st);
            f.present = true;
            f.info = FakeFlow::info(4800, 4);
        }
        tp.apply_mxl(&rd.poll(&fake, ms(t, 70)));
        fake.advance(48);
        tp.apply_mxl(&rd.poll(&fake, ms(t, 71)));
        let i = tp.info();
        assert_eq!((i["status"].as_str(), i["channel_count"].as_u64(), i["packets"].as_u64()), (Some("running"), Some(4), Some(1)));
        assert!(i["hint"].is_null());
        assert_eq!(lock(&tp.st).fold.as_ref().unwrap().packets, 1);
    }

    /// Live PipeWire point: TP_PW_REF=<object id> (network/system; run with --ignored --nocapture).
    #[test]
    #[ignore]
    fn live_pw_point() {
        let r = std::env::var("TP_PW_REF").unwrap_or_else(|_| "111".into());
        let i = set_point("pw", &r, "");
        println!("{i}");
        std::thread::sleep(Duration::from_secs(2));
        let st = status();
        println!("{st}");
        let snap = ring_snapshot(60).unwrap();
        println!("write_head={} packets={}", snap["write_head"], snap["write_seq"]);
        let c = pcm_since(0, 9600).unwrap();
        println!("pcm frames={} bytes={}", c.frames, c.payload.len());
        assert!(stop_point());
        assert_eq!(st["point"]["status"], "running");
    }

    /// Live AES67 point: TP_AOIP_REF=<session name / id / device IP> (waits for SAP).
    #[test]
    #[ignore]
    fn live_aoip_point() {
        let r = std::env::var("TP_AOIP_REF").unwrap_or_else(|_| "Desk preMO".into());
        wbs_plugin::services::aoip().ensure_started();
        set_point("aoip", &r, "");
        for _ in 0..50 {
            std::thread::sleep(Duration::from_secs(1));
            let st = status();
            if st["point"]["status"] == "running" && st["point"]["packets"].as_u64().unwrap_or(0) > 100 {
                break;
            }
        }
        let st = status();
        println!("{st}");
        let snap = ring_snapshot(60).unwrap();
        println!("peak_db ch0={} write_head={}", snap["channels"][0]["peak_db"], snap["write_head"]);
        stop_point();
    }
}
