// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.com - Ward-Beck.ca). All rights reserved.
//! Eight-channel generator flows: engine flows like the POD-33 tone, written continuously
//! into their own 8-channel rings while the engine runs (on by default, so `start()`
//! engages them), killed with the others by `stop_generators()` and re-engaged by
//! `start_generator("8ch_id")` / `start_generator("8ch_diagonal")`.
//!
//! - `wbs-flow-8ch-id`: channel n at n x 100 Hz, -18 dBFS.
//! - `wbs-flow-8ch-diagonal`: 1 kHz, -38 ... -3 dBFS rising across ch1-8, mirrored every 2 s.
//!
//! The sample math is in the Ward-Beck Generators crate (`wbs_generators::multichannel`).

use super::*;
use wbs_generators::multichannel::{ChannelId, Diagonal, CHANNELS};

/// (generator name, flow id, label)
pub(crate) const GENERATORS: [(&str, &str, &str); 2] = [
    ("8ch_id", "wbs-flow-8ch-id", "WBS 8-Channel ID 100\u{2013}800 Hz"),
    ("8ch_diagonal", "wbs-flow-8ch-diagonal", "WBS 8-Channel Diagonal"),
];

/// Frames a ring snapshot reads per bin for channels 3-8 (one 300 Hz period).
const SNAPSHOT_WINDOW: usize = 160;

/// On/off flag and worker thread of each 8-channel generator.
pub(crate) struct Generators {
    on: [AtomicBool; 2],
    threads: [Mutex<Option<JoinHandle<()>>>; 2],
}

impl Generators {
    pub(crate) fn new() -> Self {
        Generators { on: [AtomicBool::new(true), AtomicBool::new(true)], threads: [Mutex::new(None), Mutex::new(None)] }
    }
}

/// The two 8-channel engine flows (10 s rings at 48 kHz, like the others).
pub(crate) fn flows(shm_base: &str, domain_id: &str) -> Vec<Arc<MxlFlow>> {
    GENERATORS
        .iter()
        .map(|&(_, id, label)| Arc::new(MxlFlow::new(shm_base, domain_id, id, label, 48000, CHANNELS, 480000, 256, "generator")))
        .collect()
}

pub(crate) fn generators_enabled(inner: &Inner) -> Vec<(String, bool)> {
    GENERATORS.iter().zip(&inner.multi.on).map(|(g, on)| (g.0.to_string(), on.load(Ordering::Acquire))).collect()
}

/// Engages one 8-channel generator by name; any other name is ignored.
pub(crate) fn start_generator(inner: &Arc<Inner>, name: &str) {
    let Some(k) = GENERATORS.iter().position(|g| g.0 == name) else { return };
    inner.multi.on[k].store(true, Ordering::Release);
    if alive(&inner.multi.threads[k]) {
        return;
    }
    let i = inner.clone();
    *inner.multi.threads[k].lock().unwrap() = Some(std::thread::spawn(move || worker(i, k)));
}

/// Stops both 8-channel generators and silences their rings. Returns the flows cleared.
pub(crate) fn stop_generators(inner: &Inner) -> Vec<String> {
    for on in &inner.multi.on {
        on.store(false, Ordering::Release);
    }
    for slot in &inner.multi.threads {
        let t0 = Instant::now();
        while alive(slot) && t0.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    let mut cleared = Vec::new();
    for &(_, id, _) in &GENERATORS {
        if let Some(f) = inner.flows.iter().find(|f| f.flow_id == id) {
            f.clear();
            cleared.push(f.flow_id.clone());
        }
    }
    cleared
}

fn worker(inner: Arc<Inner>, k: usize) {
    const QUANTUM: usize = 256;
    let Some(flow) = inner.flows.iter().find(|f| f.flow_id == GENERATORS[k].1).cloned() else { return };
    let sr = flow.sample_rate;
    // Start at the TAI sample index so the diagonal flips on even TAI seconds.
    let at = ((current_tai_ns() as u128 * sr as u128) / 1_000_000_000u128) as u64;
    let mut id = ChannelId::new(sr);
    let mut diag = Diagonal::new(sr);
    id.set_position(at);
    diag.set_position(at);
    let interval = QUANTUM as f64 / sr as f64;
    let start = Instant::now();
    let mut n = 0u64;
    let mut bufs = vec![vec![0.0f64; QUANTUM]; CHANNELS];
    while inner.running() && inner.multi.on[k].load(Ordering::Acquire) {
        if k == 0 {
            id.fill(&mut bufs);
        } else {
            diag.fill(&mut bufs);
        }
        flow.write_planar(&bufs);
        n += 1;
        sleep_until(start, n, interval);
    }
}

impl MxlFlow {
    /// `write_frames` for any channel count: writes one planar block (one slice per ring
    /// channel, all the same length) into the ring, keeps the ch1/ch2 waveform bins,
    /// advances the write head and stamps the header.
    pub fn write_planar(&self, chans: &[Vec<f64>]) {
        self.ensure_open();
        let n = chans.iter().take(self.channel_count).map(|c| c.len()).min().unwrap_or(0);
        if n == 0 {
            return;
        }
        let _w = self.write_lock.lock().unwrap();
        let maps = self.maps.read().unwrap();
        let mut head = self.write_head.load(Ordering::Acquire);
        let buf_len = self.buffer_length;
        if self.clock_locked.load(Ordering::Relaxed) {
            let now = clock_position(current_tai_ns(), self.sample_rate, buf_len);
            let want = (now + buf_len - n % buf_len) % buf_len;
            let slip = (head as i64 - want as i64).rem_euclid(buf_len as i64);
            let slip = slip.min(buf_len as i64 - slip) as f64 / self.sample_rate as f64;
            if slip > CLOCK_SLIP_MAX_S {
                head = want;
            }
        }
        for (c, data) in chans.iter().enumerate().take(self.channel_count) {
            let data = &data[..n];
            if head + n <= buf_len {
                self.write_seg(&maps, c, head, data);
            } else {
                let l1 = buf_len - head;
                self.write_seg(&maps, c, head, &data[..l1]);
                self.write_seg(&maps, c, 0, &data[l1..]);
            }
        }

        {
            let ch0 = &chans[0][..n];
            let ch1 = &chans.get(1).unwrap_or(&chans[0])[..n];
            let mut b = self.bins.lock().unwrap();
            let fpb = buf_len as f64 / NUM_BINS as f64;
            let mut i = 0usize;
            while i < n {
                let pos = (head + i) % buf_len;
                let bi = ((pos as f64 / fpb) as usize) % NUM_BINS;
                let bin_end = buf_len.min(((bi + 1) as f64 * fpb) as usize);
                let run = (n - i).min((bin_end as i64 - pos as i64).max(1) as usize);
                let (s0, s1) = (&ch0[i..i + run], &ch1[i..i + run]);
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

    /// Ring snapshot entries for channels 3 and up (the 720 bins only cover ch1/ch2):
    /// each of the `step`-wide bins is the min/max of a short window read at its ring
    /// position, with the channel's meter.
    pub(crate) fn extra_channel_snapshots(&self, step: usize, peak: &[f64], rms: &[f64]) -> Vec<Value> {
        if self.channel_count <= 2 {
            return Vec::new();
        }
        let fpb = self.buffer_length as f64 / NUM_BINS as f64;
        let starts: Vec<usize> = (0..NUM_BINS).step_by(step.max(1)).map(|b| (b as f64 * fpb) as usize).collect();
        let window = SNAPSHOT_WINDOW.min(self.buffer_length);
        let maps = self.maps.read().unwrap();
        (2..self.channel_count)
            .map(|c| {
                let (mut lo, mut hi) = (Vec::with_capacity(starts.len()), Vec::with_capacity(starts.len()));
                for &s in &starts {
                    let v: Vec<f64> = self.read_channel(&maps, c, s, window).into_iter().map(f64::from).collect();
                    lo.push(py_round(py_min(0.0, seq_min(&v)), 5));
                    hi.push(py_round(py_max(0.0, seq_max(&v)), 5));
                }
                json!({
                    "min": f64_list(&lo),
                    "max": f64_list(&hi),
                    "peak_db": f64v(to_db(peak.get(c).copied().unwrap_or(0.0))),
                    "rms_db": f64v(to_db(rms.get(c).copied().unwrap_or(0.0))),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> String {
        let root = std::env::var("WBS_TEST_SCRATCH").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
        let p = Path::new(&root).join(format!("mxl-unit-8ch-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn eight_channel_flows_are_engine_flows() {
        let base = scratch("engine");
        let e = MxlEngine::with_base("domain-test", &base);
        for &(name, id, label) in &GENERATORS {
            let f = e.flow(id).expect(id);
            assert_eq!(f.channel_count, 8);
            assert_eq!(f.label, label);
            let def = read_json_file(&Path::new(f.flow_dir()).join("flow_def.json")).unwrap();
            assert_eq!(def["channel_count"], 8);
            assert_eq!(fs::metadata(Path::new(f.flow_dir()).join("ring.data")).unwrap().len(), 8 * 480000 * 4);
            assert!(e.generators_enabled().contains(&(name.to_string(), true)));
        }
        let rings = e.get_rings(60);
        for &(_, id, _) in &GENERATORS {
            let r = rings["flows"].as_array().unwrap().iter().find(|r| r["flow_id"] == id).expect(id);
            assert_eq!(r["channel_count"], 8);
            assert_eq!(r["channels"].as_array().unwrap().len(), 8);
            assert_eq!(r["channels"][7]["min"].as_array().unwrap().len(), 60);
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn generators_write_eight_channels_and_stop() {
        let base = scratch("run");
        let e = MxlEngine::with_base("domain-test", &base);
        e.start_generator("8ch_id");
        e.start_generator("8ch_diagonal");
        std::thread::sleep(Duration::from_millis(150));
        let cleared = e.stop_generators();
        for &(_, id, _) in &GENERATORS {
            assert!(cleared.iter().any(|c| c == id), "{id} not cleared");
        }
        assert!(e.generators_enabled().iter().all(|(_, on)| !on));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn planar_write_lands_per_channel() {
        let base = scratch("planar");
        let f = MxlFlow::new(&base, "dom", "fl8", "Flow", 48000, 8, 4800, 256, "generator");
        let mut g = ChannelId::new(48000);
        let mut bufs = vec![vec![0.0f64; 4800]; 8];
        g.fill(&mut bufs);
        f.write_planar(&bufs);
        assert_eq!(f.write_head(), 0);
        assert_eq!(f.header().channel_count, 8);
        let back = f.read_planar(0, 4800);
        assert_eq!(back.len(), 8);
        for ch in 0..8 {
            assert_eq!(back[ch][123], bufs[ch][123] as f32);
            let peak = back[ch].iter().fold(0.0f32, |m, v| m.max(v.abs())) as f64;
            assert!((20.0 * peak.log10() + 18.0).abs() < 0.01, "ch{}", ch + 1);
        }
        // pcm: planar, all 8 channels
        let c = f.read_planar(100, 10);
        assert_eq!(c.iter().map(|c| c.len()).sum::<usize>(), 80);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn diagonal_ring_rises_then_falls() {
        let base = scratch("diag");
        let f = MxlFlow::new(&base, "dom", "fl8d", "Flow", 48000, 8, 480000, 256, "generator");
        let mut g = Diagonal::new(48000);
        let mut bufs = vec![vec![0.0f64; 4800]; 8];
        // 0.1 s into the rising half, then 0.1 s into the falling half.
        g.set_position(4800);
        g.fill(&mut bufs);
        f.write_planar(&bufs);
        g.set_position(96000 + 4800);
        g.fill(&mut bufs);
        f.write_planar(&bufs);
        let db = |v: &[f32]| 20.0 * (v.iter().fold(0.0f32, |m, x| m.max(x.abs())) as f64).log10();
        let a = f.read_planar(0, 4800);
        let b = f.read_planar(4800, 4800);
        for ch in 1..8 {
            assert!(db(&a[ch]) > db(&a[ch - 1]));
            assert!(db(&b[ch]) < db(&b[ch - 1]));
        }
        assert!((db(&a[0]) + 38.0).abs() < 0.01 && (db(&a[7]) + 3.0).abs() < 0.01);
        assert!((db(&b[0]) + 3.0).abs() < 0.01 && (db(&b[7]) + 38.0).abs() < 0.01);
        let _ = fs::remove_dir_all(&base);
    }
}
