// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! AES67 transmitters: one per MXL flow this engine writes (Master Bus, POD-33,
//! ABS-1 Bit Spitter). Each reads its flow's ring behind the write head and sends
//! RTP L24 / 48 kHz / 1 ms packets to its own multicast group, announced by SAP/SDP
//! (RFC 2974 / RFC 4566) the way AES67, RAVENNA and Dante (AES67 mode) expect.
//!
//!   group     239.69.<host octet 4>.<flow index + 1>  (override base: WBS_AES67_NET=239.69.10)
//!   port      5004 (WBS_AES67_PORT), TTL 16 (WBS_AES67_TTL)
//!   format    L24 big-endian, PT 96, ptime 1 ms (48 frames), channels = flow channels
//!   clock     RTP timestamp starts on the TAI media clock (mediaclk:direct=0, ts-refclk
//!             traceable); the send rate then follows the flow's writer (rate servo)
//!   SAP       239.255.255.255:9875 every 30 s while running, deletion on stop
//!
//! Transmitters are off until asked for (POST /api/aes67, or WBS_AES67=all at start).
//! WBS_DRY_RUN=1 builds packets but never opens a socket (unless WBS_AES67_TTL=0: host-only).

use crate::mxl::{current_tai_ns, engine, wbs_host_ip};
use serde_json::{json, Value};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const PAYLOAD_TYPE: u8 = 96;
pub const PTIME_FRAMES: usize = 48; // 1 ms at 48 kHz
const SAP_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 255);
const SAP_PORT: u16 = 9875;
const SAP_INTERVAL: Duration = Duration::from_secs(30);
/// Frames kept queued before the first packet: absorbs the bursty delivery of the
/// ring writers (parec on the sound card clock, generator threads in 256-frame quanta).
const PREROLL_FRAMES: usize = 1920; // 40 ms
/// Queue above this is trimmed back to the pre-roll (the source clock ran fast).
const MAX_QUEUE_FRAMES: usize = 9600; // 200 ms

fn env_u(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

pub fn port() -> u16 {
    env_u("WBS_AES67_PORT", 5004) as u16
}

pub fn ttl() -> u32 {
    env_u("WBS_AES67_TTL", 16) as u32
}

/// No sockets under WBS_DRY_RUN=1, except with WBS_AES67_TTL=0: those packets never
/// leave the host (loopback only), so a dry-run test daemon can still be listened to.
fn dry_run() -> bool {
    crate::tap::dry_run() && ttl() != 0
}

pub fn host_v4() -> Ipv4Addr {
    wbs_host_ip().parse().unwrap_or(Ipv4Addr::LOCALHOST)
}

/// Multicast group of the flow at `index`: 239.69.<host octet 4>.<index + 1>.
pub fn group_for(index: usize) -> Ipv4Addr {
    let n = (index + 1).min(254) as u8;
    if let Ok(net) = std::env::var("WBS_AES67_NET") {
        let o: Vec<u8> = net.trim().split('.').filter_map(|x| x.parse().ok()).collect();
        if o.len() == 3 {
            return Ipv4Addr::new(o[0], o[1], o[2], n);
        }
    }
    let host = host_v4().octets();
    Ipv4Addr::new(239, 69, host[3], n)
}

/// 32-bit FNV-1a of the flow id (stable SSRC, SAP message id hash and SDP session id).
fn fnv(s: &str) -> u32 {
    s.bytes().fold(0x811c_9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x0100_0193))
}

/// SDP (RFC 4566 + AES67 / RFC 7273 clock attributes) for one transmitter.
/// RFC 7273 reference clock: the PTP leader (grandmaster identity and domain) when one
/// is heard, else "traceable".
pub fn refclk(leader: Option<(String, i64)>) -> String {
    match leader {
        Some((gm, domain)) => format!("ptp=IEEE1588-2008:{}:{domain}", gm.to_uppercase().replace(':', "-")),
        None => "ptp=IEEE1588-2008:traceable".into(),
    }
}

pub fn sdp(flow_id: &str, label: &str, group: Ipv4Addr, channels: usize, sample_rate: u32, refclk: &str) -> String {
    let host = host_v4();
    let sess = fnv(flow_id);
    let (port, ttl) = (port(), ttl());
    format!(
        "v=0\r\n\
         o=- {sess} 0 IN IP4 {host}\r\n\
         s={label} (Ward-Beck AES67)\r\n\
         i=MXL flow {flow_id}\r\n\
         c=IN IP4 {group}/{ttl}\r\n\
         t=0 0\r\n\
         a=recvonly\r\n\
         m=audio {port} RTP/AVP {PAYLOAD_TYPE}\r\n\
         a=rtpmap:{PAYLOAD_TYPE} L24/{sample_rate}/{channels}\r\n\
         a=ptime:1\r\n\
         a=maxptime:1\r\n\
         a=source-filter: incl IN IP4 {group} {host}\r\n\
         a=ts-refclk:{refclk}\r\n\
         a=mediaclk:direct=0\r\n"
    )
}

/// SAP packet (RFC 2974): v1, IPv4 origin, no auth, application/sdp payload.
pub fn sap_packet(flow_id: &str, sdp: &str, delete: bool) -> Vec<u8> {
    let mut p = Vec::with_capacity(24 + sdp.len());
    p.push(if delete { 0x24 } else { 0x20 }); // V=1, A=0, R=0, T=delete, E=0, C=0
    p.push(0); // auth length
    p.extend_from_slice(&((fnv(flow_id) & 0xffff) as u16).to_be_bytes());
    p.extend_from_slice(&host_v4().octets());
    p.extend_from_slice(b"application/sdp\0");
    p.extend_from_slice(sdp.as_bytes());
    p
}

/// RTP packet: 12-byte header + interleaved big-endian 24-bit samples.
pub fn rtp_packet(seq: u16, ts: u32, ssrc: u32, frames: &[Vec<f32>]) -> Vec<u8> {
    let nch = frames.first().map(|f| f.len()).unwrap_or(0);
    let mut p = Vec::with_capacity(12 + frames.len() * nch * 3);
    p.push(0x80); // V=2
    p.push(PAYLOAD_TYPE);
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&ts.to_be_bytes());
    p.extend_from_slice(&ssrc.to_be_bytes());
    for fr in frames {
        for &s in fr {
            let v = (s.clamp(-1.0, 1.0) as f64 * 8_388_607.0).round() as i32;
            p.extend_from_slice(&v.to_be_bytes()[1..4]);
        }
    }
    p
}

fn multicast_socket() -> std::io::Result<Socket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_multicast_ttl_v4(ttl())?;
    s.set_multicast_loop_v4(true)?; // our own SAP listener and local receivers hear it
    let host = host_v4();
    if !host.is_loopback() {
        let _ = s.set_multicast_if_v4(&host);
    }
    // DSCP EF (46) for media, as AES67 recommends
    let _ = s.set_tos(46 << 2);
    Ok(s)
}

struct Tx {
    flow_id: String,
    label: String,
    group: Ipv4Addr,
    channels: usize,
    sample_rate: u32,
    running: Arc<AtomicBool>,
    packets: Arc<AtomicU64>,
    underruns: Arc<AtomicU64>,
    trims: Arc<AtomicU64>,
    rate: Arc<AtomicU64>,
    started_at: Option<f64>,
    error: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl Tx {
    fn sdp(&self) -> String {
        sdp(&self.flow_id, &self.label, self.group, self.channels, self.sample_rate, &refclk(crate::aoip::ptp_leader()))
    }

    fn json(&self) -> Value {
        let on = self.running.load(Ordering::Acquire);
        json!({
            "flow_id": self.flow_id,
            "label": self.label,
            "running": on,
            "multicast_ip": self.group.to_string(),
            "port": port(),
            "ttl": ttl(),
            "source_ip": host_v4().to_string(),
            "payload_type": PAYLOAD_TYPE,
            "encoding": "L24",
            "sample_rate": self.sample_rate,
            "channels": self.channels,
            "ptime_ms": 1,
            "ssrc": fnv(&self.flow_id),
            "packets": self.packets.load(Ordering::Relaxed),
            "underruns": self.underruns.load(Ordering::Relaxed),
            "trims": self.trims.load(Ordering::Relaxed),
            "rate_ppm": ((f64::from_bits(self.rate.load(Ordering::Relaxed)) - 1.0) * 1e6).round() as i64,
            "started_at": self.started_at,
            "error": self.error.lock().unwrap().clone(),
            "sdp": self.sdp(),
        })
    }
}

struct State {
    txs: Vec<Tx>,
    sap_thread: Option<JoinHandle<()>>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| {
        let eng = engine();
        let txs = eng
            .flow_ids()
            .iter()
            .enumerate()
            .filter_map(|(i, id)| eng.flow(id).map(|f| (i, f)))
            .map(|(index, f)| Tx {

                flow_id: f.flow_id.clone(),
                label: short_label(&f.label),
                group: group_for(index),
                channels: f.channel_count,
                sample_rate: f.sample_rate,
                running: Arc::new(AtomicBool::new(false)),
                packets: Arc::new(AtomicU64::new(0)),
                underruns: Arc::new(AtomicU64::new(0)),
                trims: Arc::new(AtomicU64::new(0)),
                rate: Arc::new(AtomicU64::new(1.0f64.to_bits())),
                started_at: None,
                error: Arc::new(Mutex::new(None)),
                thread: None,
            })
            .collect();
        Mutex::new(State { txs, sap_thread: None })
    })
}

/// "Ward-Beck Master Bus (Float32 Stereo)" -> "Master Bus".
fn short_label(l: &str) -> String {
    let l = l.strip_prefix("Ward-Beck ").unwrap_or(l);
    l.split(" (").next().unwrap_or(l).trim().to_string()
}

/// GET /api/aes67
pub fn status() -> Value {
    let st = state().lock().unwrap();
    json!({
        "sap_group": format!("{SAP_GROUP}:{SAP_PORT}"),
        "dry_run": dry_run(),
        "transmitters": st.txs.iter().map(|t| t.json()).collect::<Vec<_>>(),
    })
}

pub fn any_running() -> bool {
    state().lock().unwrap().txs.iter().any(|t| t.running.load(Ordering::Acquire))
}

/// The sender: paces one 1 ms packet per millisecond from a queue fed by the ring.
#[allow(clippy::too_many_arguments)]
fn sender(
    flow_id: String,
    group: Ipv4Addr,
    sample_rate: u32,
    running: Arc<AtomicBool>,
    packets: Arc<AtomicU64>,
    underruns: Arc<AtomicU64>,
    trims: Arc<AtomicU64>,
    rate: Arc<AtomicU64>,
    error: Arc<Mutex<Option<String>>>,
) {
    let sock = if dry_run() {
        None
    } else {
        match multicast_socket() {
            Ok(s) => Some(s),
            Err(e) => {
                *error.lock().unwrap() = Some(format!("socket: {e}"));
                running.store(false, Ordering::Release);
                return;
            }
        }
    };
    let dest = SockAddr::from(SocketAddrV4::new(group, port()));
    let ssrc = fnv(&flow_id);
    let eng = engine();
    let mut cursor: i64 = eng.get_flow_pcm(&flow_id, -1, 256).map(|c| c.cursor).unwrap_or(0);
    let mut queue: VecDeque<Vec<f32>> = VecDeque::with_capacity(MAX_QUEUE_FRAMES * 2);
    let mut nch = 2usize;
    let mut seq: u16 = (ssrc & 0xffff) as u16;
    // AES67 media clock: RTP timestamp = TAI x sample rate (mediaclk:direct=0), and the
    // packet rate is exactly that clock (1000 packets/s). A receiver plays each packet at
    // its timestamp + its link offset (Dante: 1-10 ms), so a stamp that lags the wire is
    // dropped as late. The ring's writer runs on its own clock (parec follows the sound
    // card, 1-2 % off), so the queue absorbs the difference by slipping single frames,
    // never by moving the packet rate or the timestamps.
    let media_now = || ((current_tai_ns() as u128 * sample_rate as u128 / 1_000_000_000) & 0xffff_ffff) as u32;
    let mut ts: u32 = 0;
    let mut primed = false;
    let nominal = Duration::from_secs_f64(PTIME_FRAMES as f64 / sample_rate.max(1) as f64); // 1 ms
    let target = PREROLL_FRAMES as f64;
    let mut fill = target;
    let mut next = Instant::now();

    while running.load(Ordering::Acquire) && !crate::util::is_shutting_down() {
        // Drain whatever the ring has written since the last pass (planar f32 LE).
        if let Some(chunk) = eng.get_flow_pcm(&flow_id, cursor, 4800) {
            cursor = chunk.cursor;
            nch = chunk.channels.max(1);
            let n = chunk.frames;
            if n > 0 {
                let f32at = |c: usize, i: usize| {
                    let o = (c * n + i) * 4;
                    f32::from_le_bytes([chunk.payload[o], chunk.payload[o + 1], chunk.payload[o + 2], chunk.payload[o + 3]])
                };
                for i in 0..n {
                    queue.push_back((0..nch).map(|c| f32at(c, i)).collect());
                }
            }
        }
        if queue.len() > MAX_QUEUE_FRAMES {
            let drop = queue.len() - PREROLL_FRAMES;
            queue.drain(..drop);
            trims.fetch_add(1, Ordering::Relaxed);
        }
        if !primed {
            if queue.len() < PREROLL_FRAMES {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            primed = true;
            next = Instant::now();
            ts = media_now(); // stamped at the first send, after the pre-roll
        }

        // Send every packet that is due (catches up after a late wake-up).
        let now = Instant::now();
        while next <= now {
            // Frame slip: writer fast -> skip one frame, writer slow -> repeat one.
            if fill > target + 480.0 && queue.len() > PTIME_FRAMES + 1 {
                queue.pop_front();
            } else if fill < target - 480.0 && queue.len() >= PTIME_FRAMES {
                if let Some(f) = queue.front().cloned() {
                    queue.push_front(f);
                }
            }
            let frames: Vec<Vec<f32>> = if queue.len() >= PTIME_FRAMES {
                queue.drain(..PTIME_FRAMES).collect()
            } else {
                underruns.fetch_add(1, Ordering::Relaxed);
                let mut f: Vec<Vec<f32>> = queue.drain(..).collect();
                f.resize(PTIME_FRAMES, vec![0.0; nch]);
                f
            };
            let pkt = rtp_packet(seq, ts, ssrc, &frames);
            if let Some(s) = &sock {
                if let Err(e) = s.send_to(&pkt, &dest) {
                    *error.lock().unwrap() = Some(format!("send: {e}"));
                }
            }
            seq = seq.wrapping_add(1);
            ts = ts.wrapping_add(PTIME_FRAMES as u32);
            packets.fetch_add(1, Ordering::Relaxed);
            fill = 0.998 * fill + 0.002 * queue.len() as f64;
            // Reported as rate_ppm: how far the writer's clock is from the media clock
            let factor = (1.0 + 0.5 * (fill - target) / target).clamp(0.9, 1.1);
            rate.store(factor.to_bits(), Ordering::Relaxed);
            next += nominal;
            // Keep the stamps on TAI: re-anchor if the system clock was stepped or slewed
            // more than 10 ms away from the monotonic pacing clock.
            let drift = media_now().wrapping_sub(ts) as i32;
            if drift.unsigned_abs() > PTIME_FRAMES as u32 * 10 {
                ts = media_now();
            }
        }
        let wait = next.saturating_duration_since(Instant::now());
        std::thread::sleep(wait.min(Duration::from_millis(1)));
    }
}

fn send_sap(flow_id: &str, sdp: &str, delete: bool) {
    if dry_run() {
        return;
    }
    if let Ok(s) = multicast_socket() {
        let dest = SockAddr::from(SocketAddrV4::new(SAP_GROUP, SAP_PORT));
        let _ = s.send_to(&sap_packet(flow_id, sdp, delete), &dest);
    }
}

/// One announcer for all running transmitters (RFC 2974 interval, simplified to 30 s).
fn ensure_sap_thread(st: &mut State) {
    if st.sap_thread.as_ref().map(|h| !h.is_finished()).unwrap_or(false) {
        return;
    }
    st.sap_thread = Some(std::thread::spawn(|| loop {
        let live: Vec<(String, String)> = {
            let st = state().lock().unwrap();
            st.txs.iter().filter(|t| t.running.load(Ordering::Acquire)).map(|t| (t.flow_id.clone(), t.sdp())).collect()
        };
        if live.is_empty() || crate::util::is_shutting_down() {
            return;
        }
        for (id, sdp) in &live {
            send_sap(id, sdp, false);
        }
        let until = Instant::now() + SAP_INTERVAL;
        while Instant::now() < until {
            std::thread::sleep(Duration::from_millis(250));
            if crate::util::is_shutting_down() || !any_running() {
                return;
            }
        }
    }));
}

fn start_tx(st: &mut State, i: usize) {
    let t = &mut st.txs[i];
    if t.running.load(Ordering::Acquire) {
        return;
    }
    if let Some(h) = t.thread.take() {
        let _ = h.join();
    }
    *t.error.lock().unwrap() = None;
    t.packets.store(0, Ordering::Relaxed);
    t.underruns.store(0, Ordering::Relaxed);
    t.trims.store(0, Ordering::Relaxed);
    t.rate.store(1.0f64.to_bits(), Ordering::Relaxed);
    t.running.store(true, Ordering::Release);
    t.started_at = Some(crate::util::now_f64());
    let (id, g, sr) = (t.flow_id.clone(), t.group, t.sample_rate);
    let (r, p, u, tr, ra, e) =
        (t.running.clone(), t.packets.clone(), t.underruns.clone(), t.trims.clone(), t.rate.clone(), t.error.clone());
    t.thread = Some(std::thread::spawn(move || sender(id, g, sr, r, p, u, tr, ra, e)));
    println!("[AES67] TX {} -> {}:{} (L24/{}/{} 1 ms)", t.flow_id, t.group, port(), t.sample_rate, t.channels);
    ensure_sap_thread(st);
}

fn stop_tx(st: &mut State, i: usize) -> bool {
    let t = &mut st.txs[i];
    let was = t.running.swap(false, Ordering::AcqRel);
    if let Some(h) = t.thread.take() {
        let _ = h.join();
    }
    t.started_at = None;
    if was {
        send_sap(&t.flow_id, &t.sdp(), true);
        println!("[AES67] TX {} stopped", t.flow_id);
    }
    was
}

/// POST /api/aes67  {"flow_id": "...", "enabled": true|false}  or  {"all": true|false}.
/// Without "enabled" the named transmitter toggles.
pub fn control(body: &serde_json::Map<String, Value>) -> Result<Value, String> {
    {
        let mut st = state().lock().unwrap();
        if let Some(all) = body.get("all").and_then(|v| v.as_bool()) {
            for i in 0..st.txs.len() {
                if all {
                    start_tx(&mut st, i);
                } else {
                    stop_tx(&mut st, i);
                }
            }
        } else {
            let id = body.get("flow_id").and_then(|v| v.as_str()).ok_or("flow_id or all required")?;
            let i = st.txs.iter().position(|t| t.flow_id == id).ok_or_else(|| format!("unknown flow {id}"))?;
            let on = match body.get("enabled").and_then(|v| v.as_bool()) {
                Some(b) => b,
                None => !st.txs[i].running.load(Ordering::Acquire),
            };
            if on {
                start_tx(&mut st, i);
            } else {
                stop_tx(&mut st, i);
            }
        }
    }
    Ok(status())
}

/// Stops every transmitter and sends SAP deletions. Returns the labels that were live.
pub fn stop_all() -> Vec<String> {
    let mut st = state().lock().unwrap();
    let mut out = Vec::new();
    for i in 0..st.txs.len() {
        if stop_tx(&mut st, i) {
            out.push(format!("AES67 TX {}", st.txs[i].label));
        }
    }
    out
}

/// WBS_AES67=all (or a comma list of flow ids) starts transmitters at launch.
pub fn autostart() {
    let Ok(v) = std::env::var("WBS_AES67") else { return };
    let v = v.trim().to_string();
    if v.is_empty() || v == "0" {
        return;
    }
    let mut st = state().lock().unwrap();
    for i in 0..st.txs.len() {
        let id = st.txs[i].flow_id.clone();
        if v == "all" || v == "1" || v.split(',').any(|x| x.trim() == id) {
            start_tx(&mut st, i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtp_header_and_l24() {
        let p = rtp_packet(0x1234, 0xdeadbeef, 0x01020304, &[vec![1.0, -1.0], vec![0.0, 0.5]]);
        assert_eq!(&p[..12], &[0x80, 96, 0x12, 0x34, 0xde, 0xad, 0xbe, 0xef, 1, 2, 3, 4]);
        assert_eq!(&p[12..18], &[0x7f, 0xff, 0xff, 0x80, 0x00, 0x01]);
        assert_eq!(&p[18..21], &[0, 0, 0]);
        assert_eq!(&p[21..24], &[0x40, 0x00, 0x00]);
        assert_eq!(p.len(), 12 + 2 * 2 * 3);
    }

    #[test]
    fn sap_header() {
        let p = sap_packet("f", "v=0\r\n", false);
        assert_eq!(p[0], 0x20);
        assert_eq!(p[1], 0);
        assert!(p.windows(16).any(|w| w == b"application/sdp\0"));
        assert_eq!(sap_packet("f", "", true)[0], 0x24);
        let parsed = crate::aoip::parse_sap(&sap_packet("f", &sdp("f", "Master", Ipv4Addr::new(239, 69, 1, 1), 2, 48000, &refclk(None)), false)).unwrap();
        assert_eq!(parsed["sdp"]["multicast_ip"], "239.69.1.1");
        assert_eq!(parsed["sdp"]["port"], 5004);
    }

    #[test]
    fn labels() {
        assert_eq!(short_label("Ward-Beck Master Bus (Float32 Stereo)"), "Master Bus");
    }

    #[test]
    fn groups() {
        assert_eq!(group_for(0).octets()[0..2], [239, 69]);
        assert_eq!(group_for(2).octets()[3], 3);
    }
}
