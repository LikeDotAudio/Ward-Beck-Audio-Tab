// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.com - Ward-Beck.ca). All rights reserved.
//! Port of mxl_engine.py: MXL (EBU DMF) domains and flows in POSIX shared memory.
//! CONTRACT: the pub items below are used by other modules; keep their signatures.
//! The on-disk layout under util::shm_base() must stay byte-identical to the Python
//! (domain_def.json, flow_def.json, 64-byte "MXL1" data header, planar Float32 channels).
//!
//! - Domain & Flow hierarchy in POSIX shared memory (<shm_base>/<domain>/<flow>/)
//! - De-interleaved circular ring buffers for continuous Float32 audio (audio/float32)
//! - SMPTE 2059-1 (TAI) nanosecond timestamp indexing
//! - Lock-free ring buffer writes, read-only (PROT_READ) mappings for consumers
//! - Wrap-around slice mechanics (wrappedMultiBufferSlice)
//! - Live PipeWire/ALSA edge ingestion (parec) into MXL shared memory

// Plugged into the host: routes, services and start-up (plugin.rs).
pub mod plugin;
pub use plugin::plug;
// The 8-channel ID and diagonal generator flows.
mod multichannel;
// ABS-1 ×4 Bit Spitter: its own 8-channel flow (GET / POST /api/bitspitter8).
pub mod spitter8;

use memmap2::{Mmap, MmapMut};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(target_os = "linux")]
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use wbs_common::{self as util, PcmChunk};

/// ASCII 'MXL1'.
pub const HEADER_MAGIC: u32 = 0x4D58_4C31;
/// Exact 64-byte aligned header (struct `<IIIIQQIIIIII8s`).
pub const HEADER_SIZE: usize = 64;
pub use wbs_common::{current_tai_ns, TAI_OFFSET_SEC};
/// Waveform bin cache for the 10-second polar visualizer (72 bins/sec).
pub const NUM_BINS: usize = 720;
pub const WBS_TEST_SIGNALS_DOMAIN_DIR: &str = "domain-wbs-test-signals";
pub const WBS_UUID_PREFIX: &str = "25030";
pub const WBS_TEST_SIGNALS_DOMAIN_LABEL: &str = "Ward-Beck Test Signals";
/// Frames in each synthesized Test Signals grain.
pub const TEST_SIGNAL_FRAMES: usize = 19456;
pub const ENGINE_DOMAIN_ID: &str = "domain-wbs-dmf-01";
pub const DEFAULT_SOURCE: &str = "wbs_meter_tap.monitor";

// ---------------------------------------------------------------------------
// Python-compatible helpers
// ---------------------------------------------------------------------------

/// Python round(x, n) (correctly rounded, half-even on the exact binary value).
pub fn py_round(x: f64, n: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{:.*}", n, x).parse().unwrap_or(x)
}

/// Python min(a, b): returns a unless b < a.
fn py_min(a: f64, b: f64) -> f64 {
    if b < a { b } else { a }
}

/// Python max(a, b): returns a unless b > a.
fn py_max(a: f64, b: f64) -> f64 {
    if b > a { b } else { a }
}

fn seq_min(s: &[f64]) -> f64 {
    s.iter().skip(1).fold(s[0], |a, &b| py_min(a, b))
}

fn seq_max(s: &[f64]) -> f64 {
    s.iter().skip(1).fold(s[0], |a, &b| py_max(a, b))
}

/// Linear amplitude to dBFS (Python lambda in ring_snapshot / get_flow_levels).
fn to_db(v: f64) -> f64 {
    if v > 1e-6 { py_round(20.0 * v.log10(), 2) } else { -120.0 }
}

fn f64v(x: f64) -> Value {
    serde_json::Number::from_f64(x).map(Value::Number).unwrap_or(Value::Null)
}

fn f64_list(v: &[f64]) -> Value {
    Value::Array(v.iter().map(|&x| f64v(x)).collect())
}

/// Python truthiness of a JSON value (for `a or b`).
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|x| x != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

pub use wbs_pyshim::json::{py_float_repr, py_json_dumps};

fn write_json_file(path: &Path, v: &Value) -> std::io::Result<()> {
    fs::write(path, py_json_dumps(v, Some(2)))
}

fn read_json_file(path: &Path) -> Option<Value> {
    let s = fs::read_to_string(path).ok()?;
    serde_json::from_str(&s).ok()
}

fn is_dir(p: &Path) -> bool {
    fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false)
}

fn is_file(p: &Path) -> bool {
    fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

fn join(a: &str, b: &str) -> String {
    Path::new(a).join(b).to_string_lossy().into_owned()
}

fn list_dir(p: &Path) -> Vec<String> {
    match fs::read_dir(p) {
        Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect(),
        Err(_) => Vec::new(),
    }
}

fn bytes_to_f32(raw: &[u8]) -> Vec<f32> {
    raw.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

// ---------------------------------------------------------------------------
// Header packing
// ---------------------------------------------------------------------------

/// Packs the 64-byte ring.header (`<IIIIQQIIIIII8s`).
#[allow(clippy::too_many_arguments)]
pub fn pack_header(
    write_head: u32,
    write_seq: u64,
    tai_ns: u64,
    overrun: u32,
    underrun: u32,
    quantum: u32,
    sample_rate: u32,
    channel_count: u32,
    buffer_length: u32,
) -> [u8; HEADER_SIZE] {
    let mut h = [0u8; HEADER_SIZE];
    h[0..4].copy_from_slice(&HEADER_MAGIC.to_le_bytes());
    h[4..8].copy_from_slice(&1u32.to_le_bytes());
    h[8..12].copy_from_slice(&write_head.to_le_bytes());
    // 12..16 reserved1 = 0
    h[16..24].copy_from_slice(&write_seq.to_le_bytes());
    h[24..32].copy_from_slice(&tai_ns.to_le_bytes());
    h[32..36].copy_from_slice(&overrun.to_le_bytes());
    h[36..40].copy_from_slice(&underrun.to_le_bytes());
    h[40..44].copy_from_slice(&quantum.to_le_bytes());
    h[44..48].copy_from_slice(&sample_rate.to_le_bytes());
    h[48..52].copy_from_slice(&channel_count.to_le_bytes());
    h[52..56].copy_from_slice(&buffer_length.to_le_bytes());
    h
}

/// Decoded ring.header fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingHeader {
    pub magic: u32,
    pub version: u32,
    pub write_head: u32,
    pub write_seq: u64,
    pub tai_timestamp_ns: u64,
    pub overrun_count: u32,
    pub underrun_count: u32,
    pub quantum: u32,
    pub sample_rate: u32,
    pub channel_count: u32,
    pub buffer_length: u32,
}

pub fn unpack_header(h: &[u8]) -> RingHeader {
    let u32at = |o: usize| u32::from_le_bytes([h[o], h[o + 1], h[o + 2], h[o + 3]]);
    let u64at = |o: usize| {
        let mut b = [0u8; 8];
        b.copy_from_slice(&h[o..o + 8]);
        u64::from_le_bytes(b)
    };
    RingHeader {
        magic: u32at(0),
        version: u32at(4),
        write_head: u32at(8),
        write_seq: u64at(16),
        tai_timestamp_ns: u64at(24),
        overrun_count: u32at(32),
        underrun_count: u32at(36),
        quantum: u32at(40),
        sample_rate: u32at(44),
        channel_count: u32at(48),
        buffer_length: u32at(52),
    }
}

/// Parses a UUID string into its 16 big-endian bytes (Python uuid.UUID(s).bytes).
pub fn uuid_bytes(s: &str) -> Option<[u8; 16]> {
    let mut t = s.trim();
    if let Some(r) = t.strip_prefix("urn:") {
        t = r;
    }
    if let Some(r) = t.strip_prefix("uuid:") {
        t = r;
    }
    let hex: String = t.chars().filter(|c| !matches!(c, '-' | '{' | '}')).collect();
    if hex.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Generates the standard 2064-byte MXL data header for CBC/EBU DMF compatibility.
pub fn create_mxl_data_header(
    flow_uuid: &str,
    channel_count: u32,
    sample_rate: u64,
    quantum: u32,
    buffer_length: u32,
) -> Vec<u8> {
    let mut raw = vec![0u8; 2064];
    let put = |raw: &mut Vec<u8>, off: usize, b: &[u8]| raw[off..off + b.len()].copy_from_slice(b);
    put(&mut raw, 0, &1u32.to_le_bytes());
    put(&mut raw, 4, &2048u32.to_le_bytes());
    put(&mut raw, 8, &uuid_bytes(flow_uuid).unwrap_or([0u8; 16]));
    put(&mut raw, 0x18, &2u64.to_le_bytes()); // continuous format
    put(&mut raw, 0x20, &sample_rate.to_le_bytes()); // grainRate numerator
    put(&mut raw, 0x28, &1u64.to_le_bytes()); // grainRate denominator
    put(&mut raw, 0x30, &quantum.to_le_bytes()); // maxSyncBatchSizeHint
    put(&mut raw, 0x34, &quantum.to_le_bytes()); // maxCommitBatchSizeHint
    put(&mut raw, 0x38, &0xFFFF_FFFF_0000_0000u64.to_le_bytes());
    put(&mut raw, 0x88, &channel_count.to_le_bytes());
    put(&mut raw, 0x8C, &buffer_length.to_le_bytes());
    let now_ns = current_tai_ns();
    put(&mut raw, 0xD0, &now_ns.to_le_bytes());
    put(&mut raw, 0xD8, &now_ns.to_le_bytes());
    // 0x800: write_head = 0 (u64), write_seq = 0 (u32): already zero
    raw
}

// ---------------------------------------------------------------------------
// Ward-Beck Test Signals: host IP, UUIDs, catalog, synthesis
// ---------------------------------------------------------------------------

pub use wbs_net::{wbs_host_ip, wbs_host_ipv4};

/// Ward-Beck Test Signal UUID: "25030" once, then the IPv4 octets each zero-padded
/// to 3 digits, then the zero-padded instance number (32 digits total).
/// e.g. 192.168.1.50 instance 1 -> 25030192-1680-0105-0000-000000000001
pub fn wbs_uuid(instance: u64, ip: &str) -> String {
    let mut octets: Vec<&str> = ip.split('.').collect();
    let ok = octets.len() == 4
        && octets.iter().all(|o| !o.is_empty() && o.chars().all(|c| c.is_ascii_digit()) && o.parse::<u64>().map(|v| v <= 255).unwrap_or(false));
    if !ok {
        octets = vec!["127", "0", "0", "1"];
    }
    let ip_digits: String = octets.iter().map(|o| format!("{:03}", o.parse::<u64>().unwrap_or(0))).collect();
    let tail_len = 32 - WBS_UUID_PREFIX.len() - ip_digits.len();
    let inst = format!("{:0w$}", instance, w = tail_len);
    let inst = &inst[inst.len() - tail_len..];
    let d = format!("{}{}{}", WBS_UUID_PREFIX, ip_digits, inst);
    format!("{}-{}-{}-{}-{}", &d[0..8], &d[8..12], &d[12..16], &d[16..20], &d[20..32])
}

/// Test Signals domain UUID (instance 0).
pub fn wbs_test_signals_domain_id() -> String {
    wbs_uuid(0, wbs_host_ip())
}

/// True while some writer (this TAP included: flock is per open file) holds the lock on a
/// flow's `data` file, which is how libmxl / mxl-info tell an Active flow.
fn writer_holds_lock(data: &Path) -> bool {
    let Ok(f) = OpenOptions::new().read(true).open(data) else { return false };
    if wbs_sys::try_lock_exclusive(&f) {
        wbs_sys::unlock(&f);
        false
    } else {
        true
    }
}

/// One entry of WBS_TEST_SIGNALS_CATALOG.
#[derive(Debug, Clone)]
pub struct TestSignalFlow {
    pub uuid: String,
    pub flow_id: &'static str,
    pub label: &'static str,
    pub grouphint: &'static str,
    pub desc: &'static str,
    pub signal_type: &'static str,
}

const CATALOG_SRC: [(&str, &str, &str, &str, &str); 9] = [
    ("wbs-abb1-bit-spitter", "ABB-1-Bit-Spitter", "Ward-Beck:Bit-Spitter",
     "Ward-Beck ABB-1 Audio Bit Buddy & ABP-1 Bit Spitter (Walking 1s / PRBS-15 / Polarity)", "bitbuddy"),
    ("wbs-pod33-1k-cal", "POD-33-1kHz-Reference", "Ward-Beck:POD-33",
     "Ward-Beck POD-33 Calibrated 1 kHz Reference Tone (0 VU / -18 dBFS EBU / -20 dBFS SMPTE)", "sine_1000"),
    ("wbs-pod33-400-tone", "POD-33-400Hz-Alignment", "Ward-Beck:POD-33",
     "Ward-Beck POD-33 400 Hz Console Alignment Tone", "sine_400"),
    ("wbs-pod33-100-tone", "POD-33-100Hz-LowEnd", "Ward-Beck:POD-33",
     "Ward-Beck POD-33 100 Hz Low Frequency Reference Tone", "sine_100"),
    ("wbs-pod33-dars-silence", "POD-33-DARS-Silence", "Ward-Beck:POD-33",
     "Ward-Beck POD-33 DARS Digital Audio Reference Word Clock Silence", "silence"),
    ("wbs-pink-noise", "WBS-Pink-Noise", "Ward-Beck:Acoustic",
     "Ward-Beck Equal-Energy Per Octave Pink Noise Calibration Reference", "pink_noise"),
    ("wbs-frequency-sweep", "WBS-Frequency-Sweep", "Ward-Beck:Acoustic",
     "Ward-Beck 20 Hz to 20 kHz Logarithmic Precision Frequency Sweep", "sweep"),
    ("wbs-master-program", "WBS-Master-Audio", "Ward-Beck:Master",
     "Ward-Beck Systems Console Master Audio Program Flow", "master"),
    ("wbs-signal-tap", "WBS-Signal-Tap", "Ward-Beck:Demarc",
     "Ward-Beck Demarcation Signal Tap Point (Physical HW Tap)", "demarc"),
];

/// The catalog with UUIDs for the given host IP.
pub fn test_signals_catalog_for(ip: &str) -> Vec<TestSignalFlow> {
    CATALOG_SRC
        .iter()
        .enumerate()
        .map(|(i, &(flow_id, label, grouphint, desc, signal_type))| TestSignalFlow {
            uuid: wbs_uuid(i as u64 + 1, ip),
            flow_id,
            label,
            grouphint,
            desc,
            signal_type,
        })
        .collect()
}

/// WBS_TEST_SIGNALS_CATALOG (UUIDs from the host IP resolved at startup).
pub fn test_signals_catalog() -> &'static [TestSignalFlow] {
    static CAT: OnceLock<Vec<TestSignalFlow>> = OnceLock::new();
    CAT.get_or_init(|| test_signals_catalog_for(wbs_host_ip()))
}

/// Test Signals grains (sine, pink noise, sweep, PRBS-15, silence) come from the
/// Ward-Beck Generators crate (Generators/ submodule).
pub use wbs_generators::synthesize_test_signal_buffer;

// ---------------------------------------------------------------------------
// MxlFlow
// ---------------------------------------------------------------------------

/// Raw pointers into the four mappings (writer header/data, PROT_READ header/data).
/// The owning mappings live in `_hold`; pointers stay valid while this struct lives.
struct Maps {
    hdr_w: *mut u8,
    data_w: *mut u8,
    hdr_r: *const u8,
    data_r: *const u8,
    _hold: Vec<MapHold>,
}

#[allow(dead_code)]
enum MapHold {
    Mut(MmapMut),
    Ro(Mmap),
}

// The mappings are shared memory accessed through raw pointers; concurrent access
// is the lock-free ring contract (single writer, many readers), exactly as in Python.
unsafe impl Send for Maps {}
unsafe impl Sync for Maps {}

impl Maps {
    fn anon(data_size: usize) -> Maps {
        let mut h = MmapMut::map_anon(HEADER_SIZE).expect("anonymous header map");
        let mut d = MmapMut::map_anon(data_size.max(1)).expect("anonymous data map");
        let (hp, dp) = (h.as_mut_ptr(), d.as_mut_ptr());
        Maps { hdr_w: hp, data_w: dp, hdr_r: hp, data_r: dp, _hold: vec![MapHold::Mut(h), MapHold::Mut(d)] }
    }
}

struct Bins {
    ch0_min: Vec<f64>,
    ch0_max: Vec<f64>,
    ch1_min: Vec<f64>,
    ch1_max: Vec<f64>,
    last_bin: i64,
}

struct Meter {
    head: usize,
    peak: Vec<f64>,
    rms: Vec<f64>,
}

/// One flow this engine writes (program bus or a declared generator).
pub struct MxlFlow {
    pub flow_id: String,
    pub label: String,
    pub kind: String,
    pub domain_id: String,
    pub sample_rate: u32,
    pub channel_count: usize,
    pub buffer_length: usize,
    pub quantum: usize,
    flow_dir: String,
    def_path: String,
    header_path: String,
    data_path: String,
    data_size: usize,
    write_head: AtomicUsize,
    write_seq: AtomicU64,
    overrun_count: AtomicU32,
    underrun_count: AtomicU32,
    maps: RwLock<Maps>,
    closed: AtomicBool,
    bins: Mutex<Bins>,
    meter: Mutex<Meter>,
    write_lock: Mutex<()>,
    /// Write head phase-locked to TAI (see `clock_position`). Off in unit tests, which
    /// count frames from 0.
    clock_locked: AtomicBool,
}

/// A writer that slips further than this from the clock is snapped back to it.
const CLOCK_SLIP_MAX_S: f64 = 0.050;

/// Ring position of the sample at TAI time `tai_ns`: MXL media time, (TAI × rate) mod length.
/// With a 10 s ring the head crosses position 0 at every TAI second ending in 0, so the eye's
/// spokes 0-9 land on the downbeat of each second.
pub fn clock_position(tai_ns: u64, sample_rate: u32, buffer_length: usize) -> usize {
    let index = (tai_ns as u128 * sample_rate as u128) / 1_000_000_000u128;
    (index % buffer_length as u128) as usize
}

impl MxlFlow {
    /// Creates the flow folder under `<shm_base>/<domain_id>/<flow_id>` with flow_def.json,
    /// ring.header and ring.data, and maps them (writer RW + reader PROT_READ).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        shm_base: &str,
        domain_id: &str,
        flow_id: &str,
        label: &str,
        sample_rate: u32,
        channel_count: usize,
        buffer_length: usize,
        quantum: usize,
        kind: &str,
    ) -> MxlFlow {
        let flow_dir = join(&join(shm_base, domain_id), flow_id);
        let data_size = channel_count * buffer_length * 4;
        let f = MxlFlow {
            flow_id: flow_id.to_string(),
            label: label.to_string(),
            kind: kind.to_string(),
            domain_id: domain_id.to_string(),
            sample_rate,
            channel_count,
            buffer_length,
            quantum,
            def_path: join(&flow_dir, "flow_def.json"),
            header_path: join(&flow_dir, "ring.header"),
            data_path: join(&flow_dir, "ring.data"),
            flow_dir,
            data_size,
            write_head: AtomicUsize::new(0),
            clock_locked: AtomicBool::new(!cfg!(test)),
            write_seq: AtomicU64::new(0),
            overrun_count: AtomicU32::new(0),
            underrun_count: AtomicU32::new(0),
            maps: RwLock::new(Maps::anon(HEADER_SIZE)),
            closed: AtomicBool::new(false),
            bins: Mutex::new(Bins {
                ch0_min: vec![0.0; NUM_BINS],
                ch0_max: vec![0.0; NUM_BINS],
                ch1_min: vec![0.0; NUM_BINS],
                ch1_max: vec![0.0; NUM_BINS],
                last_bin: -1,
            }),
            meter: Mutex::new(Meter { head: 0, peak: vec![0.0; channel_count], rms: vec![0.0; channel_count] }),
            write_lock: Mutex::new(()),
        };
        let maps = match f.init_files() {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[MXL] {}: shared memory unavailable ({}); using private memory", f.flow_dir, e);
                Maps::anon(data_size)
            }
        };
        *f.maps.write().unwrap() = maps;
        f
    }

    /// Folder of this flow (Python: flow.flow_dir).
    pub fn flow_dir(&self) -> &str {
        &self.flow_dir
    }

    fn init_files(&self) -> std::io::Result<Maps> {
        fs::create_dir_all(&self.flow_dir)?;
        // 1. flow_def.json
        let flow_def = json!({
            "domain_id": self.domain_id,
            "flow_id": self.flow_id,
            "label": self.label,
            "media_type": "audio/float32",
            "sample_rate": self.sample_rate,
            "channel_count": self.channel_count,
            "buffer_length": self.buffer_length,
            "quantum": self.quantum,
            "grain_duration_ns": ((self.quantum as f64 / self.sample_rate as f64) * 1_000_000_000.0) as u64,
            "time_epoch": "SMPTE-2059-1-TAI",
            "shm_path": self.flow_dir,
            "created_at_tai_ns": current_tai_ns(),
        });
        write_json_file(Path::new(&self.def_path), &flow_def)?;

        // 2. ring.header (64 bytes)
        let hdr = pack_header(
            self.write_head.load(Ordering::Acquire) as u32,
            self.write_seq.load(Ordering::Acquire),
            current_tai_ns(),
            self.overrun_count.load(Ordering::Relaxed),
            self.underrun_count.load(Ordering::Relaxed),
            self.quantum as u32,
            self.sample_rate,
            self.channel_count as u32,
            self.buffer_length as u32,
        );
        fs::write(&self.header_path, hdr)?;

        // 3. ring.data (channel_count * buffer_length * 4 bytes), non-sparse
        let size_ok = fs::metadata(&self.data_path).map(|m| m.len() == self.data_size as u64).unwrap_or(false);
        if !size_ok {
            let mut f = File::create(&self.data_path)?;
            f.set_len(self.data_size as u64)?;
            // Linux reserves the blocks up front; elsewhere writing the last byte commits the size.
            #[cfg(target_os = "linux")]
            let r = unsafe { libc::posix_fallocate(f.as_raw_fd(), 0, self.data_size as libc::off_t) };
            #[cfg(not(target_os = "linux"))]
            let r = 1;
            if r != 0 && self.data_size > 0 {
                use std::io::Seek;
                f.seek(std::io::SeekFrom::Start(self.data_size as u64 - 1))?;
                f.write_all(&[0u8])?;
            }
        }

        // Writer maps
        let fh = OpenOptions::new().read(true).write(true).open(&self.header_path)?;
        let mut hw = unsafe { memmap2::MmapOptions::new().len(HEADER_SIZE).map_mut(&fh)? };
        let fd = OpenOptions::new().read(true).write(true).open(&self.data_path)?;
        let mut dw = unsafe { memmap2::MmapOptions::new().len(self.data_size).map_mut(&fd)? };
        // Reader maps (PROT_READ for reader isolation)
        let frh = File::open(&self.header_path)?;
        let hr = unsafe { memmap2::MmapOptions::new().len(HEADER_SIZE).map(&frh)? };
        let frd = File::open(&self.data_path)?;
        let dr = unsafe { memmap2::MmapOptions::new().len(self.data_size).map(&frd)? };
        Ok(Maps {
            hdr_w: hw.as_mut_ptr(),
            data_w: dw.as_mut_ptr(),
            hdr_r: hr.as_ptr(),
            data_r: dr.as_ptr(),
            _hold: vec![MapHold::Mut(hw), MapHold::Mut(dw), MapHold::Ro(hr), MapHold::Ro(dr)],
        })
    }

    /// Re-creates the files and maps if the flow was closed (Python: ensure_open).
    pub fn ensure_open(&self) {
        if self.closed.swap(false, Ordering::AcqRel) {
            if let Ok(m) = self.init_files() {
                *self.maps.write().unwrap() = m;
            }
        }
    }

    /// Marks the maps closed; the next access re-initialises them (Python: close()).
    /// The mappings themselves stay alive so concurrent readers never fault.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    /// Phase-lock the write head to TAI (on by default outside tests).
    pub fn set_clock_locked(&self, on: bool) {
        self.clock_locked.store(on, Ordering::Relaxed);
    }

    pub fn write_head(&self) -> usize {
        self.write_head.load(Ordering::Acquire)
    }

    pub fn write_seq(&self) -> u64 {
        self.write_seq.load(Ordering::Acquire)
    }

    /// Reads `count` frames of channel `ch` from frame `start` (wrapping) through the PROT_READ map.
    fn read_channel(&self, maps: &Maps, ch: usize, start: usize, count: usize) -> Vec<f32> {
        let l = self.buffer_length;
        let base = ch * l * 4;
        let mut out = Vec::with_capacity(count);
        let mut pos = start % l.max(1);
        let mut left = count;
        while left > 0 {
            let take = left.min(l - pos);
            let mut raw = vec![0u8; take * 4];
            unsafe {
                std::ptr::copy_nonoverlapping(maps.data_r.add(base + pos * 4), raw.as_mut_ptr(), take * 4);
            }
            out.extend(bytes_to_f32(&raw));
            left -= take;
            pos = 0;
        }
        out
    }

    fn read_raw(&self, maps: &Maps, ch: usize, start: usize, count: usize, out: &mut Vec<u8>) {
        let l = self.buffer_length;
        let base = ch * l * 4;
        let mut pos = start % l.max(1);
        let mut left = count;
        while left > 0 {
            let take = left.min(l - pos);
            let at = out.len();
            out.resize(at + take * 4, 0);
            unsafe {
                std::ptr::copy_nonoverlapping(maps.data_r.add(base + pos * 4), out.as_mut_ptr().add(at), take * 4);
            }
            left -= take;
            pos = 0;
        }
    }

    /// Planar frames [start, start+count) of the ring (wrapping), one Vec per channel,
    /// read through the read-only mapping (Python: flow._reader_mm slices).
    pub fn read_planar(&self, start: usize, count: usize) -> Vec<Vec<f32>> {
        self.ensure_open();
        let count = count.min(self.buffer_length);
        let maps = self.maps.read().unwrap();
        (0..self.channel_count).map(|c| self.read_channel(&maps, c, start, count)).collect()
    }

    fn write_seg(&self, maps: &Maps, ch: usize, pos: usize, vals: &[f64]) {
        let mut raw = Vec::with_capacity(vals.len() * 4);
        for &v in vals {
            raw.extend_from_slice(&(v as f32).to_le_bytes());
        }
        let off = ch * self.buffer_length * 4 + pos * 4;
        unsafe {
            std::ptr::copy_nonoverlapping(raw.as_ptr(), maps.data_w.add(off), raw.len());
        }
    }

    /// Writes de-interleaved samples (converted to Float32) into the circular ring,
    /// updates the 720 min/max bins, advances the write head and stamps the header.
    pub fn write_frames(&self, ch0: &[f64], ch1: &[f64]) {
        self.ensure_open();
        let n = ch0.len().min(ch1.len());
        if n == 0 {
            return;
        }
        let _w = self.write_lock.lock().unwrap();
        let maps = self.maps.read().unwrap();
        let mut head = self.write_head.load(Ordering::Acquire);
        let buf_len = self.buffer_length;
        if self.clock_locked.load(Ordering::Relaxed) {
            // These n frames end now: they belong just behind the clock's ring position.
            let now = clock_position(current_tai_ns(), self.sample_rate, buf_len);
            let want = (now + buf_len - n % buf_len) % buf_len;
            let slip = (head as i64 - want as i64).rem_euclid(buf_len as i64);
            let slip = slip.min(buf_len as i64 - slip) as f64 / self.sample_rate as f64;
            if slip > CLOCK_SLIP_MAX_S {
                head = want;
            }
        }
        let chans = [&ch0[..n], &ch1[..n]];
        for (c, data) in chans.iter().enumerate().take(self.channel_count) {
            if head + n <= buf_len {
                self.write_seg(&maps, c, head, data);
            } else {
                let l1 = buf_len - head;
                self.write_seg(&maps, c, head, &data[..l1]);
                self.write_seg(&maps, c, 0, &data[l1..]);
            }
        }

        // A bin is reset the first time the write head enters it, so each bin always
        // holds the newest ~13.9 ms of ring data at that buffer position.
        {
            let mut b = self.bins.lock().unwrap();
            let fpb = buf_len as f64 / NUM_BINS as f64;
            let mut i = 0usize;
            while i < n {
                let pos = (head + i) % buf_len;
                let bi = ((pos as f64 / fpb) as usize) % NUM_BINS;
                let bin_end = buf_len.min(((bi + 1) as f64 * fpb) as usize);
                let run = (n - i).min((bin_end as i64 - pos as i64).max(1) as usize);
                let s0 = &ch0[i..i + run];
                let s1 = &ch1[i..i + run];
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
        self.write_head.store(new_head, Ordering::Release);
        let seq = self.write_seq.fetch_add(1, Ordering::AcqRel) + 1;
        let tai = current_tai_ns();
        unsafe {
            std::ptr::copy_nonoverlapping((new_head as u32).to_le_bytes().as_ptr(), maps.hdr_w.add(8), 4);
            std::ptr::copy_nonoverlapping(seq.to_le_bytes().as_ptr(), maps.hdr_w.add(16), 8);
            std::ptr::copy_nonoverlapping(tai.to_le_bytes().as_ptr(), maps.hdr_w.add(24), 8);
        }
    }

    /// Silences the ring: zeroes the shared-memory data, waveform bins and meters.
    pub fn clear(&self) {
        self.ensure_open();
        let _w = self.write_lock.lock().unwrap();
        {
            let maps = self.maps.read().unwrap();
            unsafe { std::ptr::write_bytes(maps.data_w, 0, self.data_size) };
        }
        let mut guard = self.bins.lock().unwrap();
        let b = &mut *guard;
        for v in [&mut b.ch0_min, &mut b.ch0_max, &mut b.ch1_min, &mut b.ch1_max] {
            v.iter_mut().for_each(|x| *x = 0.0);
        }
        b.last_bin = -1;
        let mut m = self.meter.lock().unwrap();
        m.head = self.write_head();
        m.peak = vec![0.0; self.channel_count];
        m.rms = vec![0.0; self.channel_count];
    }

    /// Reads current ring state from the PROT_READ header map.
    pub fn header(&self) -> RingHeader {
        self.ensure_open();
        let maps = self.maps.read().unwrap();
        let mut h = [0u8; HEADER_SIZE];
        unsafe { std::ptr::copy_nonoverlapping(maps.hdr_r, h.as_mut_ptr(), HEADER_SIZE) };
        unpack_header(&h)
    }

    /// Python read_header() dict.
    pub fn read_header(&self) -> Value {
        let h = self.header();
        json!({
            "magic": format!("{:#x}", h.magic),
            "version": h.version,
            "write_head": h.write_head,
            "write_seq": h.write_seq,
            "tai_timestamp_ns": h.tai_timestamp_ns,
            "overrun_count": h.overrun_count,
            "underrun_count": h.underrun_count,
            "quantum": h.quantum,
            "sample_rate": h.sample_rate,
            "channel_count": h.channel_count,
            "buffer_length": h.buffer_length,
        })
    }

    /// 720-bin radial waveform of the 10-second ring, phase-aligned to NTP and TAI.
    pub fn get_waveform_bins(&self) -> Value {
        let tai_now = current_tai_ns();
        let ntp_sec = util::now_f64();
        let hdr = self.header();
        let b = self.bins.lock().unwrap();
        json!({
            "flow_id": self.flow_id,
            "sample_rate": self.sample_rate,
            "channel_count": self.channel_count,
            "buffer_length": self.buffer_length,
            "duration_sec": f64v(self.buffer_length as f64 / self.sample_rate as f64),
            "write_head": hdr.write_head,
            "write_seq": hdr.write_seq,
            "tai_timestamp_ns": tai_now,
            "ntp_sec": f64v(ntp_sec),
            "second_of_10": f64v(py_round(ntp_sec % 10.0, 4)),
            "head_sec": f64v(py_round(hdr.write_head as f64 / self.sample_rate as f64, 4)),
            "bins": NUM_BINS,
            "ch0_min": f64_list(&b.ch0_min),
            "ch0_max": f64_list(&b.ch0_max),
            "ch1_min": f64_list(&b.ch1_min),
            "ch1_max": f64_list(&b.ch1_max),
        })
    }

    /// Advances the meter reader cursor toward the write head, reading the ring
    /// through the PROT_READ mapping and integrating per-channel peak / RMS.
    pub fn meter_read(&self, max_frames: usize) {
        self.ensure_open();
        let w = self.write_head();
        let buf_len = self.buffer_length;
        let mut m = self.meter.lock().unwrap();
        let mut avail = (w + buf_len - m.head % buf_len) % buf_len;
        if avail == 0 {
            return;
        }
        if avail > max_frames {
            m.head = (w + buf_len - max_frames % buf_len) % buf_len;
            avail = max_frames;
        }
        let start = m.head;
        let maps = self.maps.read().unwrap();
        for ch in 0..self.channel_count {
            let vals = self.read_channel(&maps, ch, start, avail);
            let mut pk = f64::NEG_INFINITY;
            let mut sq = 0.0f64;
            for &v in &vals {
                let v = v as f64;
                pk = py_max(pk, v.abs());
                sq += v * v;
            }
            let rms = (sq / avail as f64).sqrt();
            // Peak: instant attack, ~1.5 s fall-back; RMS: ~300 ms ballistics
            m.peak[ch] = py_max(pk, m.peak[ch] * 0.92);
            m.rms[ch] += (rms - m.rms[ch]) * 0.35;
        }
        m.head = (start + avail) % buf_len;
    }

    /// Current meter (peak, rms) linear values per channel.
    pub fn meter_levels(&self) -> (Vec<f64>, Vec<f64>) {
        let m = self.meter.lock().unwrap();
        (m.peak.clone(), m.rms.clone())
    }

    /// Real ring state for the radial view: heads, meters and min/max bins per channel.
    pub fn ring_snapshot(&self, bins: usize) -> Value {
        let step = (NUM_BINS / bins.max(1)).max(1);
        let down = |mins: &[f64], maxs: &[f64]| {
            let mut lo = Vec::new();
            let mut hi = Vec::new();
            let mut i = 0;
            while i < NUM_BINS {
                let e = (i + step).min(NUM_BINS);
                lo.push(seq_min(&mins[i..e]));
                hi.push(seq_max(&maxs[i..e]));
                i += step;
            }
            (lo, hi)
        };
        let b = self.bins.lock().unwrap();
        let (peak, rms, meter_head) = {
            let m = self.meter.lock().unwrap();
            (m.peak.clone(), m.rms.clone(), m.head)
        };
        let pairs = [(&b.ch0_min, &b.ch0_max), (&b.ch1_min, &b.ch1_max)];
        let mut ch = Vec::new();
        for (c, (mins, maxs)) in pairs.iter().enumerate().take(self.channel_count) {
            let (lo, hi) = down(mins, maxs);
            ch.push(json!({
                "min": f64_list(&lo),
                "max": f64_list(&hi),
                "peak_db": f64v(to_db(peak[c])),
                "rms_db": f64v(to_db(rms[c])),
            }));
        }
        ch.extend(self.extra_channel_snapshots(step, &peak, &rms));
        json!({
            "flow_id": self.flow_id,
            "label": self.label,
            "kind": self.kind,
            "sample_rate": self.sample_rate,
            "channel_count": self.channel_count,
            "buffer_length": self.buffer_length,
            "write_head": self.write_head(),
            "meter_head": meter_head,
            "write_seq": self.write_seq(),
            "channels": ch,
        })
    }

    /// mxlFlowReaderGetSamples(): an mxlWrappedMultiBufferSlice, split in two at the seam.
    pub fn get_samples(&self, tail_index: i64, count: usize) -> Value {
        let hdr = self.header();
        let w_head = hdr.write_head as usize;
        let buf_len = self.buffer_length;
        let tail = tail_index.rem_euclid(buf_len as i64) as usize;
        let count = count.min(buf_len);
        let lead_frames = (w_head + buf_len - tail) % buf_len;
        let is_wrapped = tail + count > buf_len;
        let (s1_len, s2_len) = if is_wrapped { (buf_len - tail, count - (buf_len - tail)) } else { (count, 0) };
        let (ch0, ch1) = {
            let maps = self.maps.read().unwrap();
            (self.read_channel(&maps, 0, tail, count), self.read_channel(&maps, 1, tail, count))
        };
        let to_list = |v: &[f32]| Value::Array(v.iter().map(|&x| f64v(x as f64)).collect());
        json!({
            "flow_id": self.flow_id,
            "sample_rate": self.sample_rate,
            "channel_count": self.channel_count,
            "buffer_length": self.buffer_length,
            "quantum": self.quantum,
            "write_head": w_head,
            "tail_index": tail,
            "requested_count": count,
            "lead_frames": lead_frames,
            "lead_ms": f64v(py_round((lead_frames as f64 / self.sample_rate as f64) * 1000.0, 2)),
            "is_wrapped": is_wrapped,
            "slice1": {"start": tail, "length": s1_len},
            "slice2": if is_wrapped { json!({"start": 0, "length": s2_len}) } else { Value::Null },
            "samples_ch0": to_list(&ch0),
            "samples_ch1": to_list(&ch1),
            "tai_timestamp_ns": hdr.tai_timestamp_ns,
        })
    }
}

// ---------------------------------------------------------------------------
// MxlEngine
// ---------------------------------------------------------------------------

struct ExtEntry {
    at: f64,
    label: Value,
    sr: u64,
    nch: usize,
    chans: Arc<Vec<Vec<f32>>>,
}

struct Inner {
    base: String,
    domain_id: String,
    flows: Vec<Arc<MxlFlow>>, // master, tone, bit spitter
    tone_enabled: AtomicBool,
    bit_enabled: AtomicBool,
    tone_thread: Mutex<Option<JoinHandle<()>>>,
    bit_thread: Mutex<Option<JoinHandle<()>>>,
    bridge_thread: Mutex<Option<JoinHandle<()>>>,
    meter_thread: Mutex<Option<JoinHandle<()>>>,
    bridge_child: Mutex<Option<Child>>,
    shutdown: AtomicBool,
    active_source: Mutex<String>,
    lock_fds: Mutex<Vec<File>>,
    ext_cache: Mutex<HashMap<String, ExtEntry>>,
    multi: multichannel::Generators,
}

pub struct MxlEngine {
    inner: Arc<Inner>,
}

/// The process-wide engine (Python: MXL_ENGINE = MxlEngine()).
pub fn engine() -> &'static MxlEngine {
    static ENGINE: OnceLock<MxlEngine> = OnceLock::new();
    ENGINE.get_or_init(|| MxlEngine::with_base(ENGINE_DOMAIN_ID, &util::shm_base()))
}

fn alive(slot: &Mutex<Option<JoinHandle<()>>>) -> bool {
    slot.lock().unwrap().as_ref().map(|h| !h.is_finished()).unwrap_or(false)
}

fn terminate(child: &Child) {
    wbs_sys::kill(child.id(), wbs_sys::SIGTERM);
}

/// SIGTERM, then reap (SIGKILL after 1 s) so no zombie parec is left behind.
fn terminate_and_reap(mut child: Child) {
    terminate(&child);
    let t0 = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) if t0.elapsed() > Duration::from_secs(1) => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn sleep_until(start: Instant, k: u64, interval: f64) {
    let target = start + Duration::from_secs_f64(interval * k as f64);
    let now = Instant::now();
    if target > now {
        std::thread::sleep(target - now);
    }
}

fn release_locks(fds: &mut Vec<File>) {
    for f in fds.drain(..) {
        wbs_sys::unlock(&f);
    }
}

impl Inner {
    fn running(&self) -> bool {
        !self.shutdown.load(Ordering::Acquire) && !util::is_shutting_down()
    }

    fn master(&self) -> &Arc<MxlFlow> {
        &self.flows[0]
    }

    fn tone(&self) -> &Arc<MxlFlow> {
        &self.flows[1]
    }

    /// POD-33 400 / 100 Hz (the router's MXL67-3), written with the 1 kHz tone.
    fn low_tone(&self) -> &Arc<MxlFlow> {
        &self.flows[2]
    }

    fn bit_spitter(&self) -> &Arc<MxlFlow> {
        &self.flows[3]
    }

    fn edge_bridge_worker(self: Arc<Self>) {
        const QUANTUM: usize = 256;
        const BYTES: usize = QUANTUM * 2 * 2; // 2 channels, S16_LE
        while self.running() {
            let src = self.active_source.lock().unwrap().clone();
            let spawned = Command::new("parec")
                .arg(format!("--device={}", src))
                .arg("--format=s16le")
                .arg("--rate=48000")
                .arg("--channels=2")
                // 10 ms here would pin the whole PipeWire graph to a 256-sample cycle and
                // underrun the speakers; 40 ms only batches the 256-frame reads below.
                .arg("--latency-msec=40")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            if let Ok(mut child) = spawned {
                let mut out = child.stdout.take();
                *self.bridge_child.lock().unwrap() = Some(child);
                if let Some(out) = out.as_mut() {
                    let mut buf = [0u8; BYTES];
                    let mut ch0 = vec![0.0f64; QUANTUM];
                    let mut ch1 = vec![0.0f64; QUANTUM];
                    while self.running() {
                        if out.read_exact(&mut buf).is_err() {
                            break;
                        }
                        for i in 0..QUANTUM {
                            let l = i16::from_le_bytes([buf[i * 4], buf[i * 4 + 1]]);
                            let r = i16::from_le_bytes([buf[i * 4 + 2], buf[i * 4 + 3]]);
                            ch0[i] = l as f64 / 32768.0;
                            ch1[i] = r as f64 / 32768.0;
                        }
                        self.master().write_frames(&ch0, &ch1);
                    }
                }
                drop(out);
                let child = self.bridge_child.lock().unwrap().take();
                if let Some(c) = child {
                    terminate_and_reap(c);
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Continuously synthesizes calibrated 1 kHz 0 VU (-18 dBFS) tone into the tone flow
    /// (POD-33 tone from the Ward-Beck Generators crate).
    fn tone_worker(self: Arc<Self>) {
        const QUANTUM: usize = 256;
        let sample_rate = 48000.0f64;
        let mut tone = wbs_generators::Tone::new(1000.0, -18.0, sample_rate);
        let mut low_hz = low_tone_hz();
        let mut low = wbs_generators::Tone::new(low_hz as f64, -18.0, sample_rate);
        let interval = QUANTUM as f64 / sample_rate;
        let start = Instant::now();
        let mut k = 0u64;
        let mut ch = vec![0.0f64; QUANTUM];
        let mut lo = vec![0.0f64; QUANTUM];
        while self.running() && self.tone_enabled.load(Ordering::Acquire) {
            tone.fill(&mut ch);
            self.tone().write_frames(&ch, &ch);
            if low_tone_hz() != low_hz {
                low_hz = low_tone_hz();
                low = wbs_generators::Tone::new(low_hz as f64, -18.0, sample_rate);
            }
            low.fill(&mut lo);
            self.low_tone().write_frames(&lo, &lo);
            k += 1;
            sleep_until(start, k, interval);
        }
    }

    /// ABP-1 walking 1s (Ward-Beck Generators crate): one bit per second, 1 kHz bipolar
    /// square at that bit's weight (-6, -12 ... -60 dBFS), steps locked to ring position.
    fn bit_spitter_worker(self: Arc<Self>) {
        const QUANTUM: usize = 256;
        let sample_rate = 48000usize;
        let mut walk = wbs_generators::WalkingOnes::new(sample_rate, 10);
        let interval = QUANTUM as f64 / sample_rate as f64;
        let start = Instant::now();
        let mut k = 0u64;
        let mut ch = vec![0.0f64; QUANTUM];
        let flow = self.bit_spitter().clone();
        while self.running() && self.bit_enabled.load(Ordering::Acquire) {
            walk.fill(&mut ch, flow.write_head(), flow.buffer_length);
            flow.write_frames(&ch, &ch);
            k += 1;
            sleep_until(start, k, interval);
        }
    }

    fn meter_worker(self: Arc<Self>) {
        while self.running() {
            for f in &self.flows {
                f.meter_read(4800);
            }
            std::thread::sleep(Duration::from_secs_f64(1.0 / 30.0));
        }
    }
}

static LOW_TONE_HZ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(400);

/// The POD-33 low tone flow's frequency (400 or 100 Hz, the router's MXL67-3 switch).
pub fn low_tone_hz() -> u32 {
    LOW_TONE_HZ.load(Ordering::Relaxed)
}

pub fn set_low_tone_hz(hz: u32) -> Result<u32, String> {
    if hz != 400 && hz != 100 {
        return Err("hz must be 400 or 100".into());
    }
    LOW_TONE_HZ.store(hz, Ordering::Relaxed);
    Ok(hz)
}

impl MxlEngine {
    /// Builds an engine rooted at `shm_base` (Python: MxlEngine(domain_id) with SHM_BASE_DIR),
    /// creating the three engine flows and publishing the Ward-Beck Test Signals domain.
    pub fn with_base(domain_id: &str, shm_base: &str) -> MxlEngine {
        let mk = |id: &str, label: &str, kind: &str| {
            Arc::new(MxlFlow::new(shm_base, domain_id, id, label, 48000, 2, 480000, 256, kind))
        };
        let mut flows = vec![
            mk("wbs-flow-master-stereo", "Ward-Beck Master Bus (Float32 Stereo)", "program"),
            mk("wbs-flow-ref-tone", "Ward-Beck POD-33 Precision Tone Flow (Float32 Stereo)", "generator"),
            // The router's transmitters in order (MXL67-1 … 4): AES67 groups .1 … .4 follow this list
            mk("wbs-flow-ref-tone-low", "Ward-Beck POD-33 400 / 100 Hz Tone Flow (Float32 Stereo)", "generator"),
            mk(
                "wbs-flow-bit-spitter",
                "Ward-Beck ABB-1 Audio Bit Buddy & ABP-1 Bit Spitter Flow (Float32 Stereo)",
                "generator",
            ),
        ];
        flows.extend(multichannel::flows(shm_base, domain_id));
        let e = MxlEngine {
            inner: Arc::new(Inner {
                base: shm_base.to_string(),
                domain_id: domain_id.to_string(),
                flows,
                tone_enabled: AtomicBool::new(true),
                bit_enabled: AtomicBool::new(true),
                tone_thread: Mutex::new(None),
                bit_thread: Mutex::new(None),
                bridge_thread: Mutex::new(None),
                meter_thread: Mutex::new(None),
                bridge_child: Mutex::new(None),
                shutdown: AtomicBool::new(false),
                active_source: Mutex::new(DEFAULT_SOURCE.to_string()),
                lock_fds: Mutex::new(Vec::new()),
                ext_cache: Mutex::new(HashMap::new()),
                multi: multichannel::Generators::new(),
            }),
        };
        e.publish_wbs_test_signals_domain();
        e
    }

    pub fn domain_id(&self) -> String {
        self.inner.domain_id.clone()
    }

    /// Shared-memory root this engine writes under.
    pub fn shm_base_dir(&self) -> String {
        self.inner.base.clone()
    }

    pub fn active_source(&self) -> String {
        self.inner.active_source.lock().unwrap().clone()
    }

    /// Python generators_enabled: [("tone", on), ("bit_spitter", on)], then the 8-channel ones.
    pub fn generators_enabled(&self) -> Vec<(String, bool)> {
        let mut g = vec![
            ("tone".into(), self.inner.tone_enabled.load(Ordering::Acquire)),
            ("bit_spitter".into(), self.inner.bit_enabled.load(Ordering::Acquire)),
        ];
        g.extend(multichannel::generators_enabled(&self.inner));
        g
    }

    pub fn flow(&self, flow_id: &str) -> Option<Arc<MxlFlow>> {
        self.inner.flows.iter().find(|f| f.flow_id == flow_id).cloned()
            .or_else(|| spitter8::live_flow().filter(|f| f.flow_id == flow_id))
    }

    pub fn master_flow(&self) -> Arc<MxlFlow> {
        self.inner.master().clone()
    }

    pub fn tone_flow(&self) -> Arc<MxlFlow> {
        self.inner.tone().clone()
    }

    pub fn bit_spitter_flow(&self) -> Arc<MxlFlow> {
        self.inner.bit_spitter().clone()
    }

    pub fn flow_ids(&self) -> Vec<String> {
        self.inner.flows.iter().chain(spitter8::live_flow().as_ref()).map(|f| f.flow_id.clone()).collect()
    }

    /// Creates and populates the 'Ward-Beck Test Signals' MXL domain (domain_def.json,
    /// options.json, flow_def.json + 2064-byte data header + channels per catalog flow),
    /// cross-publishes into the CBC domain_1 (and nested) when present, and takes an
    /// advisory flock on each data file so libmxl / mxl-info report the flows Active.
    pub fn publish_wbs_test_signals_domain(&self) {
        let inner = &self.inner;
        let mut fds = inner.lock_fds.lock().unwrap();
        release_locks(&mut fds);

        let base = inner.base.as_str();
        let wbs_domain_dir = join(base, WBS_TEST_SIGNALS_DOMAIN_DIR);
        let _ = fs::create_dir_all(&wbs_domain_dir);

        let dom_id = wbs_test_signals_domain_id();
        let dom_def = json!({
            "id": dom_id,
            "label": WBS_TEST_SIGNALS_DOMAIN_LABEL,
            "description": "Ward-Beck Systems Broadcast Calibration & Precision Test Signal Domain (POD-33, ABB-1 Bit Spitter, Sweep, Noise, DARS)",
        });
        let options_def = json!({
            "urn:x-mxl:option:history_duration/v1.0":
                ((TEST_SIGNAL_FRAMES as f64 / 48000.0) * 1_000_000_000.0) as u64,
        });
        let write_domain = |d: &str| {
            let _ = write_json_file(&Path::new(d).join("domain_def.json"), &dom_def)
                .and_then(|_| write_json_file(&Path::new(d).join("options.json"), &options_def));
        };
        write_domain(&wbs_domain_dir);

        // Convenience symlink: <base>/wbs-test-signals -> domain-wbs-test-signals
        let symlink_path = join(base, "wbs-test-signals");
        if fs::metadata(&symlink_path).is_err() {
            let _ = wbs_sys::symlink(WBS_TEST_SIGNALS_DOMAIN_DIR, &symlink_path);
        }

        let mut targets: Vec<(String, Value, Value)> =
            vec![(wbs_domain_dir.clone(), dom_def["id"].clone(), dom_def["label"].clone())];
        let cbc_domain_dir = join(base, "domain_1");
        if is_dir(Path::new(&cbc_domain_dir)) {
            let mut cbc_id = json!("61ef9b5c-98c1-4f98-9def-1d61ee9a4fdb");
            let mut cbc_label = json!("exercise-4");
            let cdef_path = join(&cbc_domain_dir, "domain_def.json");
            if is_file(Path::new(&cdef_path)) {
                if let Some(Value::Object(c)) = read_json_file(Path::new(&cdef_path)) {
                    if let Some(v) = c.get("id") {
                        cbc_id = v.clone();
                    }
                    if let Some(v) = c.get("label") {
                        cbc_label = v.clone();
                    }
                }
            }
            targets.push((cbc_domain_dir.clone(), cbc_id, cbc_label));
            let nested = join(&cbc_domain_dir, WBS_TEST_SIGNALS_DOMAIN_DIR);
            let _ = fs::create_dir_all(&nested);
            write_domain(&nested);
            targets.push((nested, dom_def["id"].clone(), dom_def["label"].clone()));
        }

        // Remove stale Ward-Beck flow folders (legacy e4a78120-* or 25030* from another IP).
        let catalog = test_signals_catalog();
        let current: Vec<String> = catalog.iter().map(|f| format!("{}.mxl-flow", f.uuid)).collect();
        for (dir, _, _) in &targets {
            for name in list_dir(Path::new(dir)) {
                if name.ends_with(".mxl-flow")
                    && (name.starts_with("e4a78120-") || name.starts_with(WBS_UUID_PREFIX))
                    && !current.contains(&name)
                {
                    let _ = fs::remove_dir_all(Path::new(dir).join(&name));
                }
            }
        }

        for flw in catalog {
            let audio = synthesize_test_signal_buffer(flw.signal_type, TEST_SIGNAL_FRAMES, 48000);
            for (dir, tdom_id, tdom_label) in &targets {
                let flw_dir = Path::new(dir).join(format!("{}.mxl-flow", flw.uuid));
                let _ = fs::create_dir_all(&flw_dir);
                let fdef = json!({
                    "id": flw.uuid,
                    "description": flw.desc,
                    "tags": {"urn:x-nmos:tag:grouphint/v1.0": [flw.grouphint]},
                    "format": "urn:x-nmos:format:audio",
                    "label": flw.label,
                    "parents": [],
                    "media_type": "audio/float32",
                    "sample_rate": {"numerator": 48000, "denominator": 1},
                    "channel_count": 2,
                    "bit_depth": 32,
                    "domain_id": tdom_id,
                    "domain_label": tdom_label,
                });
                let _ = write_json_file(&flw_dir.join("flow_def.json"), &fdef);
                let data_file = flw_dir.join("data");
                let _ = fs::write(&data_file, create_mxl_data_header(&flw.uuid, 2, 48000, 480, TEST_SIGNAL_FRAMES as u32));
                let _ = fs::write(flw_dir.join("channels"), &audio);
                if let Ok(f) = OpenOptions::new().read(true).write(true).open(&data_file) {
                    if wbs_sys::try_lock_exclusive(&f) {
                        fds.push(f);
                    }
                }
            }
        }
    }

    pub fn start(&self, source_device: &str) {
        let inner = &self.inner;
        *inner.active_source.lock().unwrap() = source_device.to_string();
        inner.shutdown.store(false, Ordering::Release);
        for f in &inner.flows {
            f.ensure_open();
        }
        self.publish_wbs_test_signals_domain();

        if !alive(&inner.bridge_thread) {
            let i = inner.clone();
            *inner.bridge_thread.lock().unwrap() = Some(std::thread::spawn(move || i.edge_bridge_worker()));
        }
        for (name, on) in self.generators_enabled() {
            if on {
                self.start_generator(&name);
            }
        }
        if !alive(&inner.meter_thread) {
            let i = inner.clone();
            *inner.meter_thread.lock().unwrap() = Some(std::thread::spawn(move || i.meter_worker()));
        }
        spitter8::on_engine_start(&inner.base, &inner.domain_id);
    }

    /// Stops all workers, terminates the parec bridge, closes the flows and releases flocks.
    pub fn stop(&self) {
        let inner = &self.inner;
        inner.shutdown.store(true, Ordering::Release);
        if let Some(c) = inner.bridge_child.lock().unwrap().as_ref() {
            terminate(c);
        }
        for f in &inner.flows {
            f.close();
        }
        spitter8::on_engine_stop();
        release_locks(&mut inner.lock_fds.lock().unwrap());
    }

    /// Restarts the parec bridge on a new source device.
    pub fn set_source(&self, source_device: &str) {
        let mut cur = self.inner.active_source.lock().unwrap();
        if *cur != source_device {
            *cur = source_device.to_string();
            drop(cur);
            if let Some(c) = self.inner.bridge_child.lock().unwrap().as_ref() {
                terminate(c);
            }
        }
    }

    /// (Re-)engages an internal ring generator ('tone' or 'bit_spitter').
    pub fn start_generator(&self, name: &str) {
        let inner = &self.inner;
        let (flag, slot): (&AtomicBool, &Mutex<Option<JoinHandle<()>>>) = match name {
            "tone" => (&inner.tone_enabled, &inner.tone_thread),
            "bit_spitter" => (&inner.bit_enabled, &inner.bit_thread),
            _ => return multichannel::start_generator(inner, name),
        };
        flag.store(true, Ordering::Release);
        if alive(slot) {
            return;
        }
        let i = inner.clone();
        let h = if name == "tone" {
            std::thread::spawn(move || i.tone_worker())
        } else {
            std::thread::spawn(move || i.bit_spitter_worker())
        };
        *slot.lock().unwrap() = Some(h);
    }

    /// Kills every internal ring generator and silences its ring. Returns the flows cleared.
    pub fn stop_generators(&self) -> Vec<String> {
        let inner = &self.inner;
        inner.tone_enabled.store(false, Ordering::Release);
        inner.bit_enabled.store(false, Ordering::Release);
        for slot in [&inner.tone_thread, &inner.bit_thread] {
            let t0 = Instant::now();
            while alive(slot) && t0.elapsed() < Duration::from_secs(1) {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        let mut cleared = Vec::new();
        for f in [inner.tone(), inner.bit_spitter(), inner.low_tone()] {
            f.clear();
            cleared.push(f.flow_id.clone());
        }
        cleared.extend(multichannel::stop_generators(inner));
        cleared
    }

    /// Ring snapshot of every flow this engine writes (program + declared generators).
    pub fn get_rings(&self, bins: usize) -> Value {
        json!({
            "tai_ns": current_tai_ns(),
            "ntp_sec": f64v(util::now_f64()),
            "flows": self.inner.flows.iter().chain(spitter8::live_flow().as_ref()).map(|f| f.ring_snapshot(bins)).collect::<Vec<_>>(),
        })
    }

    /// Structured MXL layer status across all registered flows.
    pub fn get_telemetry(&self) -> Value {
        let flows_info: Vec<Value> = self
            .inner
            .flows
            .iter()
            .map(|f| {
                let h = f.header();
                json!({
                    "flow_id": f.flow_id,
                    "label": f.label,
                    "media_type": "audio/float32",
                    "sample_rate": f.sample_rate,
                    "channel_count": f.channel_count,
                    "buffer_length": f.buffer_length,
                    "quantum": f.quantum,
                    "write_head": h.write_head,
                    "write_seq": h.write_seq,
                    "tai_timestamp_ns": h.tai_timestamp_ns,
                    "overrun_count": h.overrun_count,
                    "underrun_count": h.underrun_count,
                    "shm_path": f.flow_dir,
                })
            })
            .collect();
        let domains = self.scan_all_domains();
        let tai = current_tai_ns();
        let ntp_now = util::now_f64();
        json!({
            "domain_id": self.inner.domain_id,
            "shm_base_dir": self.inner.base,
            "active_source": self.active_source(),
            "flows": flows_info,
            "domains": domains,
            "tai_epoch_ns": tai,
            "ntp_sec": f64v(ntp_now),
            "second_of_10": f64v(py_round(ntp_now % 10.0, 4)),
            "buffer_duration_sec": 10.0,
        })
    }

    /// {domain_label: {id, label, path, description?, flows: [flow_def + domain_label, domain_id, folder, path, flow_id]}}
    pub fn scan_all_domains(&self) -> Value {
        let mut domains = Map::new();
        let base = Path::new(&self.inner.base);
        if !is_dir(base) {
            return Value::Object(domains);
        }
        for entry in list_dir(base) {
            let dom_path = base.join(&entry);
            if !is_dir(&dom_path) {
                continue;
            }
            let dom_path_s = dom_path.to_string_lossy().into_owned();
            let mut meta = Map::new();
            meta.insert("id".into(), json!(entry));
            meta.insert("label".into(), json!(entry));
            meta.insert("path".into(), json!(dom_path_s));
            meta.insert("flows".into(), json!([]));
            let ddp = dom_path.join("domain_def.json");
            if is_file(&ddp) {
                if let Some(Value::Object(d)) = read_json_file(&ddp) {
                    meta.insert("id".into(), d.get("id").cloned().unwrap_or_else(|| json!(entry)));
                    meta.insert("label".into(), d.get("label").cloned().unwrap_or_else(|| json!(entry)));
                    meta.insert("description".into(), d.get("description").cloned().unwrap_or_else(|| json!("")));
                }
            }
            let mut flows = Vec::new();
            for item in list_dir(&dom_path) {
                let item_path = dom_path.join(&item);
                if !is_dir(&item_path) {
                    continue;
                }
                let fdp = item_path.join("flow_def.json");
                if !is_file(&fdp) {
                    continue;
                }
                if let Some(Value::Object(mut fdef)) = read_json_file(&fdp) {
                    fdef.insert("domain_label".into(), meta["label"].clone());
                    fdef.insert("domain_id".into(), meta["id"].clone());
                    fdef.insert("folder".into(), json!(item));
                    fdef.insert("path".into(), json!(item_path.to_string_lossy()));
                    // MXL convention: a flow is Active while its writer holds the lock on `data`
                    fdef.insert("active".into(), json!(writer_holds_lock(&item_path.join("data"))));
                    let fid = [fdef.get("id"), fdef.get("flow_id")]
                        .into_iter()
                        .flatten()
                        .find(|v| truthy(v))
                        .cloned()
                        .unwrap_or_else(|| json!(item.replace(".mxl-flow", "")));
                    fdef.insert("flow_id".into(), fid);
                    flows.push(Value::Object(fdef));
                }
            }
            meta.insert("flows".into(), Value::Array(flows));
            let key = match &meta["label"] {
                Value::String(s) => s.clone(),
                other => py_json_dumps(other, None),
            };
            domains.insert(key, Value::Object(meta));
        }
        Value::Object(domains)
    }

    fn find_external(&self, flow_id: &str) -> Option<Value> {
        if let Value::Object(domains) = self.scan_all_domains() {
            for dom in domains.values() {
                if let Some(Value::Array(flows)) = dom.get("flows") {
                    for f in flows {
                        let hit = ["flow_id", "id", "folder"]
                            .iter()
                            .any(|k| f.get(*k).and_then(|v| v.as_str()) == Some(flow_id));
                        if hit {
                            return Some(f.clone());
                        }
                    }
                }
            }
        }
        None
    }

    /// New audio of one flow since the caller's cursor, planar little-endian Float32.
    /// Engine flows read their ring behind the write head; Test Signals grains play on a
    /// wall clock (cursor = absolute frame count). since < 0 starts at "now" with no frames.
    pub fn get_flow_pcm(&self, flow_id: &str, since: i64, max_frames: usize) -> Option<PcmChunk> {
        let max_frames = max_frames.clamp(256, 48000);
        if let Some(f) = self.flow(flow_id) {
            f.ensure_open();
            let l = f.buffer_length as i64;
            let w = f.write_head() as i64;
            let nch = f.channel_count;
            if since < 0 {
                return Some(PcmChunk { payload: Vec::new(), cursor: w, channels: nch, sample_rate: f.sample_rate, frames: 0 });
            }
            let mut since = since.rem_euclid(l);
            let mut avail = (w - since).rem_euclid(l);
            if avail > max_frames as i64 {
                since = (w - max_frames as i64).rem_euclid(l);
                avail = max_frames as i64;
            }
            let mut out = Vec::with_capacity(nch * avail as usize * 4);
            {
                let maps = f.maps.read().unwrap();
                for c in 0..nch {
                    f.read_raw(&maps, c, since as usize, avail as usize, &mut out);
                }
            }
            return Some(PcmChunk { payload: out, cursor: w, channels: nch, sample_rate: f.sample_rate, frames: avail as usize });
        }

        self.get_flow_levels(flow_id); // fills / refreshes the grain cache
        let (sr, chans) = {
            let cache = self.inner.ext_cache.lock().unwrap();
            let e = cache.get(flow_id)?;
            (e.sr, e.chans.clone())
        };
        if chans.is_empty() || chans.iter().any(|c| c.is_empty()) {
            return None;
        }
        let now = (util::now_f64() * sr as f64) as i64;
        let empty = PcmChunk { payload: Vec::new(), cursor: now, channels: chans.len(), sample_rate: sr as u32, frames: 0 };
        if since < 0 {
            return Some(empty);
        }
        let mut since = since;
        let mut avail = now - since;
        if avail <= 0 {
            return Some(empty);
        }
        if avail > max_frames as i64 {
            since = now - max_frames as i64;
            avail = max_frames as i64;
        }
        let avail = avail as usize;
        let mut out = Vec::with_capacity(chans.len() * avail * 4);
        for ch in chans.iter() {
            let n = ch.len();
            let mut start = since.rem_euclid(n as i64) as usize;
            let mut got = 0;
            while got < avail {
                let take = (avail - got).min(n - start);
                for v in &ch[start..start + take] {
                    out.extend_from_slice(&v.to_le_bytes());
                }
                got += take;
                start = 0;
            }
        }
        Some(PcmChunk { payload: out, cursor: now, channels: chans.len(), sample_rate: sr as u32, frames: avail })
    }

    /// Per-channel peak / RMS (dBFS) of one MXL flow. Engine flows report their meter
    /// read head; external flows (Test Signals grains) are metered over a 100 ms window
    /// that walks the flow's channels buffer in real time.
    pub fn get_flow_levels(&self, flow_id: &str) -> Value {
        if let Some(f) = self.flow(flow_id) {
            let (peak, rms) = f.meter_levels();
            return json!({
                "flow_id": flow_id,
                "label": f.label,
                "channel_count": f.channel_count,
                "peak_db": peak.iter().map(|&v| f64v(to_db(v))).collect::<Vec<_>>(),
                "rms_db": rms.iter().map(|&v| f64v(to_db(v))).collect::<Vec<_>>(),
            });
        }

        let fresh = {
            let cache = self.inner.ext_cache.lock().unwrap();
            cache.get(flow_id).map(|e| util::now_f64() - e.at <= 5.0).unwrap_or(false)
        };
        if !fresh {
            let target = match self.find_external(flow_id) {
                Some(t) => t,
                None => {
                    return json!({"flow_id": flow_id, "error": "flow not found", "channel_count": 0, "peak_db": [], "rms_db": []})
                }
            };
            let num_u64 = |v: &Value| v.as_u64().or_else(|| v.as_f64().map(|x| x as u64));
            let sr = match target.get("sample_rate") {
                Some(Value::Object(o)) => o.get("numerator").and_then(num_u64).unwrap_or(48000),
                Some(v) if truthy(v) => num_u64(v).unwrap_or(48000),
                _ => 48000,
            };
            let nch = target
                .get("channel_count")
                .filter(|v| truthy(v))
                .and_then(num_u64)
                .unwrap_or(2)
                .max(1) as usize;
            let mut chans: Vec<Vec<f32>> = Vec::new();
            if let Some(p) = target.get("path").and_then(|v| v.as_str()) {
                let path = Path::new(p).join("channels");
                if is_file(&path) {
                    if let Ok(raw) = fs::read(&path) {
                        if raw.len() % 4 == 0 {
                            let floats = bytes_to_f32(&raw);
                            let n = floats.len() / nch;
                            chans = (0..nch).map(|c| floats[c * n..(c + 1) * n].to_vec()).collect();
                        }
                    }
                }
            }
            let entry = ExtEntry {
                at: util::now_f64(),
                label: target.get("label").cloned().unwrap_or_else(|| json!(flow_id)),
                sr,
                nch,
                chans: Arc::new(chans),
            };
            self.inner.ext_cache.lock().unwrap().insert(flow_id.to_string(), entry);
        }

        let cache = self.inner.ext_cache.lock().unwrap();
        let e = match cache.get(flow_id) {
            Some(e) => e,
            None => return json!({"flow_id": flow_id, "error": "flow not found", "channel_count": 0, "peak_db": [], "rms_db": []}),
        };
        let mut peaks = Vec::new();
        let mut rmss = Vec::new();
        for ch in e.chans.iter() {
            let n = ch.len();
            let win = n.min((e.sr / 10) as usize);
            if n == 0 || win == 0 {
                peaks.push(f64v(-120.0));
                rmss.push(f64v(-120.0));
                continue;
            }
            let start = ((util::now_f64() * e.sr as f64) as u64 % n as u64) as usize;
            let mut pk = f64::NEG_INFINITY;
            let mut sq = 0.0f64;
            for k in 0..win {
                let v = ch[(start + k) % n] as f64;
                pk = py_max(pk, v.abs());
                sq += v * v;
            }
            peaks.push(f64v(to_db(pk)));
            rmss.push(f64v(to_db((sq / win as f64).sqrt())));
        }
        json!({
            "flow_id": flow_id,
            "label": e.label,
            "channel_count": e.nch,
            "peak_db": peaks,
            "rms_db": rmss,
        })
    }

    /// Reads the circular audio buffer of an external MXL flow (e.g. CBC Test-Audio-1).
    pub fn get_external_flow_waveform(&self, flow_id: &str) -> Value {
        let tai_now = current_tai_ns();
        let ntp_sec = util::now_f64();
        let bins = NUM_BINS;
        let mut ch0_min = vec![0.0f64; bins];
        let mut ch0_max = vec![0.0f64; bins];
        let mut ch1_min = vec![0.0f64; bins];
        let mut ch1_max = vec![0.0f64; bins];
        let mut buf_len = TEST_SIGNAL_FRAMES;
        let mut srate: u64 = 48000;
        if let Some(t) = self.find_external(flow_id) {
            if let Some(Value::Object(o)) = t.get("sample_rate") {
                srate = o.get("numerator").and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|x| x as u64))).unwrap_or(48000);
            }
            if let Some(p) = t.get("path").and_then(|v| v.as_str()) {
                let cf = Path::new(p).join("channels");
                if is_file(&cf) {
                    if let Ok(raw) = fs::read(&cf) {
                        if raw.len() % 4 == 0 {
                            let floats: Vec<f64> = bytes_to_f32(&raw).into_iter().map(|v| v as f64).collect();
                            let n = floats.len() / 2;
                            buf_len = if n > 0 { n } else { TEST_SIGNAL_FRAMES };
                            let ch0 = &floats[..n];
                            let ch1 = &floats[n..];
                            let chunk = (n / bins).max(1);
                            let sl = |v: &[f64], b: usize| -> (usize, usize) {
                                let s = (b * chunk).min(v.len());
                                (s, ((b + 1) * chunk).min(v.len()))
                            };
                            for b in 0..bins {
                                let (a, z) = sl(ch0, b);
                                if z > a {
                                    ch0_min[b] = py_round(seq_min(&ch0[a..z]), 4);
                                    ch0_max[b] = py_round(seq_max(&ch0[a..z]), 4);
                                }
                                let (a, z) = sl(ch1, b);
                                if z > a {
                                    ch1_min[b] = py_round(seq_min(&ch1[a..z]), 4);
                                    ch1_max[b] = py_round(seq_max(&ch1[a..z]), 4);
                                }
                            }
                        }
                    }
                }
            }
        }
        let sr = if srate == 0 { 1.0 } else { srate as f64 };
        json!({
            "flow_id": flow_id,
            "sample_rate": srate,
            "channel_count": 2,
            "buffer_length": buf_len,
            "duration_sec": f64v(py_round(buf_len as f64 / sr, 3)),
            "write_head": buf_len,
            "write_seq": 1,
            "tai_timestamp_ns": tai_now,
            "ntp_sec": f64v(ntp_sec),
            "second_of_10": f64v(py_round(ntp_sec % 10.0, 4)),
            "head_sec": f64v(py_round(buf_len as f64 / sr, 4)),
            "bins": bins,
            "ch0_min": f64_list(&ch0_min),
            "ch0_max": f64_list(&ch0_max),
            "ch1_min": f64_list(&ch1_min),
            "ch1_max": f64_list(&ch1_max),
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> String {
        let root = std::env::var("WBS_TEST_SCRATCH").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
        let p = Path::new(&root).join(format!("mxl-unit-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn header_packing() {
        let h = pack_header(7936, 22531, 0x18ea_8926_2f5b_5000, 0, 0, 256, 48000, 2, 480000);
        assert_eq!(h.len(), 64);
        assert_eq!(&h[0..4], b"1LXM");
        // bytes captured from the Python ring.header
        let want: [u8; 56] = [
            0x31, 0x4c, 0x58, 0x4d, 0x01, 0, 0, 0, 0x00, 0x1f, 0, 0, 0, 0, 0, 0, 0x03, 0x58, 0, 0, 0, 0, 0, 0, 0x00, 0x50,
            0x5b, 0x2f, 0x26, 0x89, 0xea, 0x18, 0, 0, 0, 0, 0, 0, 0, 0, 0x00, 0x01, 0, 0, 0x80, 0xbb, 0, 0, 0x02, 0, 0, 0,
            0x00, 0x53, 0x07, 0x00,
        ];
        assert_eq!(&h[..56], &want[..]);
        assert_eq!(&h[56..], &[0u8; 8]);
        let u = unpack_header(&h);
        assert_eq!(u.magic, HEADER_MAGIC);
        assert_eq!(u.write_head, 7936);
        assert_eq!(u.write_seq, 22531);
        assert_eq!(u.buffer_length, 480000);
    }

    #[test]
    fn data_header_layout() {
        let d = create_mxl_data_header("25030044-0440-4416-8000-000000000001", 2, 48000, 480, 19456);
        assert_eq!(d.len(), 2064);
        assert_eq!(&d[0..8], &[1, 0, 0, 0, 0, 8, 0, 0]);
        assert_eq!(&d[8..24], &[0x25, 0x03, 0x00, 0x44, 0x04, 0x40, 0x44, 0x16, 0x80, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(&d[0x18..0x20], &2u64.to_le_bytes());
        assert_eq!(&d[0x20..0x30], &[0x80, 0xbb, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&d[0x30..0x40], &[0xe0, 1, 0, 0, 0xe0, 1, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]);
        assert_eq!(&d[0x88..0x90], &[2, 0, 0, 0, 0, 0x4c, 0, 0]);
        assert_eq!(&d[0xd0..0xd8], &d[0xd8..0xe0]);
        assert!(d[0x800..].iter().all(|&b| b == 0));
    }

    #[test]
    fn uuid_format() {
        assert_eq!(wbs_uuid(1, "192.168.1.50"), "25030192-1680-0105-0000-000000000001");
        assert_eq!(wbs_uuid(0, "44.44.44.8"), "25030044-0440-4400-8000-000000000000");
        assert_eq!(wbs_uuid(9, "bogus"), "25030127-0000-0000-1000-000000000009");
        assert_eq!(wbs_uuid(3, "1.2.3.256"), wbs_uuid(3, "127.0.0.1"));
        let cat = test_signals_catalog_for("44.44.16.8");
        assert_eq!(cat.len(), 9);
        assert_eq!(cat[7].uuid, "25030044-0440-1600-8000-000000000008");
        assert_eq!(cat[0].flow_id, "wbs-abb1-bit-spitter");
    }

    #[test]
    fn test_signal_synthesis() {
        let n = TEST_SIGNAL_FRAMES;
        for t in ["sine_1000", "sine_400", "sine_100", "silence", "pink_noise", "sweep", "bitbuddy", "master", "demarc"] {
            let b = synthesize_test_signal_buffer(t, n, 48000);
            assert_eq!(b.len(), 155_648, "{}", t);
            let f = bytes_to_f32(&b);
            // interleaved stereo, both channels identical
            assert!(f.chunks(2).all(|p| p[0].to_bits() == p[1].to_bits()), "{}", t);
            assert!(f.iter().all(|v| v.abs() <= 0.1 + 1e-6), "{}", t);
        }
        let s = bytes_to_f32(&synthesize_test_signal_buffer("sine_1000", 64, 48000));
        assert_eq!(s[0], 0.0);
        assert_eq!(s[2], (0.1 * (2.0 * std::f64::consts::PI * 1000.0 / 48000.0).sin()) as f32);
        let bb = bytes_to_f32(&synthesize_test_signal_buffer("bitbuddy", 4, 48000));
        // lfsr 0x7ACE: bit0^bit1 = 0^1 = 1 -> +amp
        assert_eq!(bb[0], 0.1f32);
        // matches the Python-written grain on this host when present
        let py = "/dev/shm/mxl/domain-wbs-test-signals";
        for (i, t) in [(1, "bitbuddy"), (2, "sine_1000"), (7, "sweep"), (5, "silence")] {
            if let Ok(rd) = fs::read_dir(py) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if name.ends_with(&format!("{:015}.mxl-flow", i)) {
                        let got = fs::read(e.path().join("channels")).unwrap();
                        assert_eq!(got, synthesize_test_signal_buffer(t, n, 48000), "{}", t);
                    }
                }
            }
        }
    }

    #[test]
    fn python_formatting() {
        assert_eq!(py_float_repr(0.0), "0.0");
        assert_eq!(py_float_repr(-0.0), "-0.0");
        assert_eq!(py_float_repr(10.0), "10.0");
        assert_eq!(py_float_repr(1e-5), "1e-05");
        assert_eq!(py_float_repr(0.0001), "0.0001");
        assert_eq!(py_float_repr(1e16), "1e+16");
        assert_eq!(py_float_repr(1795398043.5198543), "1795398043.5198543");
        assert_eq!(py_float_repr(-0.10902614146471024), "-0.10902614146471024");
        assert_eq!(py_round(0.125, 2), 0.12);
        assert_eq!(py_round(-0.00001, 4).to_bits(), (-0.0f64).to_bits());
        assert_eq!(py_round(2.6453333, 4), 2.6453);
        let v = json!({"a": [1, 2.5, "x\u{b7}"], "b": {}, "c": []});
        assert_eq!(py_json_dumps(&v, None), "{\"a\": [1, 2.5, \"x\\u00b7\"], \"b\": {}, \"c\": []}");
        assert_eq!(py_json_dumps(&json!({"k": [1]}), Some(2)), "{\n  \"k\": [\n    1\n  ]\n}");
    }

    #[test]
    fn flow_ring_bins_and_slices() {
        let base = scratch("flow");
        let f = MxlFlow::new(&base, "dom", "fl", "Flow", 48000, 2, 4800, 256, "program");
        assert_eq!(fs::metadata(format!("{}/dom/fl/ring.data", base)).unwrap().len(), 38400);
        assert_eq!(fs::read(format!("{}/dom/fl/ring.header", base)).unwrap().len(), 64);
        let def: Value = serde_json::from_str(&fs::read_to_string(format!("{}/dom/fl/flow_def.json", base)).unwrap()).unwrap();
        assert_eq!(def["grain_duration_ns"], 5333333);
        let keys: Vec<_> = def.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys[0], "domain_id");
        assert_eq!(keys[11], "created_at_tai_ns");

        let ch0: Vec<f64> = (0..4000).map(|i| (i as f64) / 8000.0).collect();
        let ch1: Vec<f64> = ch0.iter().map(|v| -v).collect();
        f.write_frames(&ch0, &ch1);
        f.write_frames(&ch0[..1000], &ch1[..1000]); // wraps: head 4000 -> 200
        assert_eq!(f.write_head(), 200);
        assert_eq!(f.write_seq(), 2);
        let h = f.header();
        assert_eq!((h.write_head, h.write_seq), (200, 2));

        // Planar wrap read: frames 4790..4810 -> ch0[790..800] of 2nd write then ch0[800..810]
        let p = f.read_planar(4790, 20);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0][0], (790.0f64 / 8000.0) as f32);
        assert_eq!(p[0][10], (800.0f64 / 8000.0) as f32);
        assert_eq!(p[1][10], (-800.0f64 / 8000.0) as f32);

        let s = f.get_samples(4790, 20);
        assert_eq!(s["is_wrapped"], true);
        assert_eq!(s["slice1"], json!({"start": 4790, "length": 10}));
        assert_eq!(s["slice2"], json!({"start": 0, "length": 10}));
        assert_eq!(s["lead_frames"], 210);
        let s = f.get_samples(-10, 5);
        assert_eq!(s["tail_index"], 4790);
        assert_eq!(s["slice2"], Value::Null);

        // waveform bins: frames_per_bin = 6.666..; bin 0 last refreshed by the wrap write
        let wf = f.get_waveform_bins();
        assert_eq!(wf["bins"], 720);
        assert_eq!(wf["duration_sec"].as_f64(), Some(0.1));
        let max0 = wf["ch0_max"].as_array().unwrap();
        assert_eq!(max0[0].as_f64(), Some(py_round(806.0 / 8000.0, 5)));
        assert_eq!(wf["ch1_max"][0].as_f64(), Some(0.0));

        // meter reader integrates the newest window
        f.meter_read(4800);
        let (pk, _) = f.meter_levels();
        assert!(pk[0] > 0.12 && pk[0] < 0.126);
        let rs = f.ring_snapshot(60);
        assert_eq!(rs["channels"][0]["min"].as_array().unwrap().len(), 60);
        assert_eq!(rs["meter_head"], 200);

        f.clear();
        assert!(f.read_planar(0, 4800)[0].iter().all(|&v| v == 0.0));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn engine_publish_and_pcm_cursor() {
        let base = scratch("engine");
        fs::create_dir_all(format!("{}/domain_1", base)).unwrap();
        fs::write(format!("{}/domain_1/domain_def.json", base), "{\"id\":\"x-id\",\"label\":\"exercise-4\"}").unwrap();
        let e = MxlEngine::with_base("domain-wbs-dmf-01", &base);
        let dom = format!("{}/{}", base, WBS_TEST_SIGNALS_DOMAIN_DIR);
        assert_eq!(fs::read_to_string(format!("{}/options.json", dom)).unwrap(),
                   "{\n  \"urn:x-mxl:option:history_duration/v1.0\": 405333333\n}");
        let uuid1 = &test_signals_catalog()[0].uuid;
        for d in [dom.clone(), format!("{}/domain_1", base), format!("{}/domain_1/{}", base, WBS_TEST_SIGNALS_DOMAIN_DIR)] {
            let fd = format!("{}/{}.mxl-flow", d, uuid1);
            assert_eq!(fs::metadata(format!("{}/data", fd)).unwrap().len(), 2064);
            assert_eq!(fs::metadata(format!("{}/channels", fd)).unwrap().len(), 155_648);
        }
        let cbc: Value = serde_json::from_str(&fs::read_to_string(format!("{}/domain_1/{}.mxl-flow/flow_def.json", base, uuid1)).unwrap()).unwrap();
        assert_eq!(cbc["domain_id"], "x-id");

        let doms = e.scan_all_domains();
        assert!(doms.get("Ward-Beck Test Signals").is_some());
        assert!(doms.get("exercise-4").is_some());
        assert_eq!(doms["domain-wbs-dmf-01"]["flows"].as_array().unwrap().len(), e.flow_ids().len());

        // engine flow cursor maths
        let m = e.master_flow();
        let c = e.get_flow_pcm("wbs-flow-master-stereo", -1, 9600).unwrap();
        assert_eq!((c.cursor, c.frames, c.payload.len()), (0, 0, 0));
        let ch: Vec<f64> = (0..1000).map(|i| i as f64 / 1000.0).collect();
        m.write_frames(&ch, &ch);
        let c = e.get_flow_pcm("wbs-flow-master-stereo", 0, 9600).unwrap();
        assert_eq!((c.cursor, c.frames, c.channels, c.payload.len()), (1000, 1000, 2, 8000));
        assert_eq!(&c.payload[4..8], &(0.001f64 as f32).to_le_bytes());
        let c = e.get_flow_pcm("wbs-flow-master-stereo", 1000, 9600).unwrap();
        assert_eq!(c.frames, 0);
        let c = e.get_flow_pcm("wbs-flow-master-stereo", 0, 10).unwrap(); // max clamps to 256
        assert_eq!(c.frames, 256);
        assert_eq!(&c.payload[0..4], &(0.744f64 as f32).to_le_bytes());

        // Test Signals: wall-clock absolute-frame cursor
        let id = test_signals_catalog()[1].uuid.clone();
        let c = e.get_flow_pcm(&id, -1, 9600).unwrap();
        assert_eq!(c.frames, 0);
        assert!(c.cursor > 0);
        let c2 = e.get_flow_pcm(&id, c.cursor - 480, 9600).unwrap();
        assert!(c2.frames >= 480 && c2.frames < 9600);
        assert_eq!(c2.payload.len(), c2.frames * 2 * 4);
        let c3 = e.get_flow_pcm(&id, c.cursor - 1_000_000, 9600).unwrap();
        assert_eq!(c3.frames, 9600);
        let c4 = e.get_flow_pcm(&id, c.cursor + 100_000, 9600).unwrap();
        assert_eq!(c4.frames, 0);
        assert!(e.get_flow_pcm("nope", 0, 9600).is_none());

        let lv = e.get_flow_levels(&id);
        assert_eq!(lv["label"], "POD-33-1kHz-Reference");
        assert_eq!(lv["channel_count"], 2);
        let wf = e.get_external_flow_waveform(&id);
        assert_eq!(wf["buffer_length"], 19456);
        assert_eq!(wf["duration_sec"].as_f64(), Some(0.405));
        assert_eq!(e.get_flow_levels("nope")["error"], "flow not found");

        let t = e.get_telemetry();
        let keys: Vec<_> = t.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["domain_id", "shm_base_dir", "active_source", "flows", "domains", "tai_epoch_ns", "ntp_sec", "second_of_10", "buffer_duration_sec"]);
        let r = e.get_rings(60);
        assert_eq!(r["flows"].as_array().unwrap().len(), e.flow_ids().len());
        assert_eq!(e.stop_generators()[..2], ["wbs-flow-ref-tone", "wbs-flow-bit-spitter"]);
        e.stop();
        let _ = fs::remove_dir_all(&base);
    }

    /// Manual: WBS_SHM_BASE=<dir> cargo test --lib mxl::tests::live_engine -- --ignored
    /// Runs the real engine (without parec if unavailable) for 2 s for file comparison.
    #[test]
    #[ignore]
    fn live_engine() {
        let e = engine();
        e.start("wbs_meter_tap.monitor");
        std::thread::sleep(Duration::from_secs(2));
        println!("{}", py_json_dumps(&e.get_rings(4), None));
        println!("{}", py_json_dumps(&e.get_flow_levels("wbs-flow-ref-tone"), None));
        e.stop();
        std::thread::sleep(Duration::from_millis(700));
    }

    #[test]
    fn the_write_head_is_phase_locked_to_tai() {
        // 10 s ring at 48 kHz: TAI ...0.000 s is position 0, ...3.5 s is 3.5 × 48000.
        assert_eq!(clock_position(1_700_000_000_000_000_000, 48000, 480_000), 0);
        assert_eq!(clock_position(1_700_000_003_500_000_000, 48000, 480_000), 168_000);
        let base = scratch("clock");
        let f = MxlFlow::new(&base, "dom", "fl", "Flow", 48000, 2, 480_000, 256, "program");
        f.set_clock_locked(true);
        let ch = vec![0.0f64; 480];
        f.write_frames(&ch, &ch);
        let now = clock_position(current_tai_ns(), 48000, 480_000);
        let off = (f.write_head() as i64 - now as i64).rem_euclid(480_000);
        assert!(off.min(480_000 - off) < 2400, "head {} vs clock {}", f.write_head(), now);
    }

}
