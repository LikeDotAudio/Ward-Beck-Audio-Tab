// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
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

use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::Read;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::aoip::py;
use crate::util::{now_f64, run_cmd, PcmChunk};

pub const RING_PACKETS: usize = 10;
pub const RING_SECONDS: f64 = 10.0; // every test point (ALSA, PipeWire, MXL, AoIP): 10 s ring
pub const MAX_CHANNELS: usize = 16;
pub const FLOW_ID: &str = "wbs-test-point";

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
// Test point
// ---------------------------------------------------------------------------

struct TpState {
    source: String, // resolved description of what is tapped
    status: String,
    error: Option<String>,
    hint: Option<String>,
    fix: Value, // {title, why, options:[{label, note, command}]}: only the user can run it
    ring: Option<TapRing>,
    sample_rate: u32,
    channels: usize,
    packet_frames: usize,
    lost: u64,
}

struct TestPoint {
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
    fn new(kind: &str, reference: &str, label: &str) -> Shared {
        Arc::new(TestPoint {
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
                sample_rate: 48000,
                channels: 2,
                packet_frames: 0,
                lost: 0,
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
        let h = std::thread::Builder::new().name(format!("test-point-{}", self.kind)).spawn(move || me.run());
        if let Ok(h) = h {
            *lock(&self.thread) = Some(h);
        }
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(pid) = *lock(&self.child_pid) {
            crate::sys::kill(pid, crate::sys::SIGTERM);
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
            crate::sys::kill(child.id(), crate::sys::SIGTERM);
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
            if let Some(r) = s.ring.as_mut() {
                r.write(&planar);
            }
        }
        *lock(&self.child_pid) = None;
        let _ = child.kill();
        let _ = child.wait();
        Ok(())
    }

    // -- MXL --
    fn run_mxl(&self) -> Result<(), PyErr> {
        let flow = crate::mxl::engine()
            .flow(&self.reference)
            .ok_or_else(|| PyErr::lookup(format!("MXL flow {} is not in this domain", self.reference)))?;
        let channels = flow.channel_count;
        {
            let mut s = lock(&self.st);
            s.sample_rate = flow.sample_rate;
            s.channels = channels;
            s.packet_frames = flow.quantum;
            s.source = format!("{} \u{b7} {} (shared memory)", flow.flow_id, flow.domain_id);
            s.ring = Some(TapRing::new(channels, flow.quantum, flow.sample_rate, RING_SECONDS));
            s.status = "running".into();
        }
        let l = flow.buffer_length;
        if l == 0 || flow.sample_rate == 0 {
            return Err(PyErr { class: "ZeroDivisionError", msg: "integer modulo by zero".into() });
        }
        let ring_len = lock(&self.st).ring.as_ref().map(|r| r.length).unwrap_or(0);
        let mut cursor = flow.write_head();
        let interval = flow.quantum as f64 / flow.sample_rate as f64;
        while !self.stopped() {
            std::thread::sleep(Duration::from_secs_f64(interval));
            let w = flow.write_head();
            let mut avail = (w + l - cursor % l) % l;
            if avail == 0 {
                continue;
            }
            avail = avail.min(ring_len);
            let start = (w + l - avail % l) % l;
            let planar = flow.read_planar(start, avail);
            let mut s = lock(&self.st);
            if let Some(r) = s.ring.as_mut() {
                r.write(&planar);
            }
            drop(s);
            cursor = w;
        }
        Ok(())
    }

    // -- AES67 / RAVENNA RTP --
    fn resolve_sdp(&self) -> Option<Map<String, Value>> {
        let snap = crate::aoip::snapshot();
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
            st.ring = Some(TapRing::new(channels, packet_frames, sample_rate, RING_SECONDS));
        }

        use socket2::{Domain, InterfaceIndexOrAddress, Protocol, SockAddr, Socket, Type};
        let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        sock.set_reuse_address(true)?;
        crate::sys::reuse_port(&sock);
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
            if let Some(r) = st.ring.as_mut() {
                r.write(&planar);
            }
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
        let s = lock(&self.st);
        let r = s.ring.as_ref()?;
        let chans = r.snapshot(bins);
        let head = r.head as i64;
        let mut d = match self.info_locked(&s) {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        d.insert("flow_id".into(), json!(FLOW_ID));
        d.insert("label".into(), json!(format!("TEST POINT \u{b7} {}", self.label)));
        d.insert("kind".into(), json!("testpoint"));
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

fn active() -> &'static Mutex<Option<Shared>> {
    static A: OnceLock<Mutex<Option<Shared>>> = OnceLock::new();
    A.get_or_init(|| Mutex::new(None))
}

fn current() -> Option<Shared> {
    lock(active()).clone()
}

pub fn set_point(kind: &str, reference: &str, label: &str) -> Value {
    stop_point();
    let tp = TestPoint::new(kind, reference, label);
    *lock(active()) = Some(tp.clone());
    tp.start();
    std::thread::sleep(Duration::from_millis(150));
    tp.info()
}

pub fn stop_point() -> bool {
    let tp = lock(active()).take();
    match tp {
        Some(tp) => {
            tp.stop();
            true
        }
        None => false,
    }
}

pub fn status() -> Value {
    let tp = current();
    let mut m = Map::new();
    m.insert("active".into(), json!(tp.is_some()));
    m.insert("point".into(), tp.map(|t| t.info()).unwrap_or(Value::Null));
    Value::Object(m)
}

pub fn ring_snapshot(bins: usize) -> Option<Value> {
    current()?.ring_snapshot(bins)
}

/// New ring audio since the since cursor (ring position; < 0 = start now, no frames), newest
/// max_frames at most. Same semantics as the /pcm route in ward-beck-audio-tab.py.
pub fn pcm_since(since: i64, max_frames: usize) -> Option<PcmChunk> {
    let tp = current()?;
    let s = lock(&tp.st);
    s.ring.as_ref()?.pcm_since(since, max_frames)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let tp = TestPoint::new("nmos", "x", "");
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
    fn ring_snapshot_contract() {
        let tp = TestPoint::new("pw", "111", "lbl");
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
                "buffer_length", "write_head", "meter_head", "write_seq", "channels"
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
        crate::aoip::start(Arc::new(AtomicBool::new(false)));
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
