// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.com - Ward-Beck.ca). All rights reserved.
//! AES67 transmitters: one per engine MXL flow, the router's MXL67-1 … 4 (Repeater,
//! POD-33 1 kHz, POD-33 400 / 100 Hz, Bit Spitter), and the loop outs MXL67-5 / 6 that
//! re-send the network / MoIP receivers' AMS-81 stereo fold-downs (read through the
//! receivers service). A loop out whose receiver is taking one of this TAP's own groups
//! stays silent ("loop: receiving its own stream"). Each reads its flow's ring behind the write head and sends
//! RTP L24 / 48 kHz / 1 ms packets to its own multicast group, announced by SAP/SDP
//! (RFC 2974 / RFC 4566) the way AES67, RAVENNA and Dante (AES67 mode) expect.
//!
//!   group     239.69.<host octet 4>.<flow index + 1>  (override base: WBS_AES67_NET=239.69.10)
//!   port      5004 (WBS_AES67_PORT), TTL 16 (WBS_AES67_TTL)
//!   format    L24 big-endian, PT 96, ptime 1 ms (48 frames), channels = flow channels
//!   clock     RTP timestamp starts on the TAI media clock (mediaclk:direct=0, ts-refclk
//!             traceable); the send rate then follows the flow's writer (rate servo)
//!   SAP       239.255.255.255:9875 every 30 s while running, deletion on stop
//!   Bonjour   RAVENNA style: _rtsp._tcp.local. with subtype _ravenna_session, one record
//!             per running transmitter, pointing at a small RTSP responder (OPTIONS and
//!             DESCRIBE of rtsp://host:port/by-name/<name> or /by-id/<1-6>) on TCP
//!             WBS_AES67_RTSP_PORT (9010). No port, no Bonjour: SAP carries on alone.
//!   name      each transmitter has a name (default: the flow's short label), the SDP's s=.
//!             Renaming bumps the SDP version (o=), withdraws the old SAP announcement,
//!             announces the new one at once and re-registers the Bonjour record.
//!
//! Transmitters are off until asked for (POST /api/aes67, or WBS_AES67=all at start).
//! WBS_DRY_RUN=1 builds packets but never opens a socket (unless WBS_AES67_TTL=0: host-only),
//! and then runs no mDNS and no RTSP either.

// Plugged into the host: routes, services and start-up (plugin.rs).
pub mod plugin;
pub use plugin::plug;

use wbs_common::current_tai_ns;
use wbs_plugin::services;
use serde_json::{json, Value};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
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
/// The engine flows that transmit, in the router's order (MXL67-1 … 4 = groups .1 … .4).
/// Other flows (the 8-channel Bit Spitter, the multichannel test flows) do not.
pub const ENGINE_FLOWS: [&str; 4] =
    ["wbs-flow-master-stereo", "wbs-flow-ref-tone", "wbs-flow-ref-tone-low", "wbs-flow-bit-spitter"];
/// The loop outs, MXL67-5 / 6 (groups .5 / .6): the receivers' AMS-81 fold-downs, 48 kHz stereo.
pub const LOOP_OUTS: [(&str, &str); 2] = [("wbs-network-ams81", "Network loop out"), ("wbs-moip-ams81", "MoIP loop out")];
const LOOP_ERROR: &str = "loop: receiving its own stream";
/// RAVENNA's Bonjour type: _rtsp._tcp with the _ravenna_session subtype.
const MDNS_TYPE: &str = "_ravenna_session._sub._rtsp._tcp.local.";
const NAME_MAX: usize = 64;

fn env_u(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

pub fn port() -> u16 {
    env_u("WBS_AES67_PORT", 5004) as u16
}

pub fn rtsp_port() -> u16 {
    env_u("WBS_AES67_RTSP_PORT", 9010) as u16
}

pub fn ttl() -> u32 {
    env_u("WBS_AES67_TTL", 16) as u32
}

/// No sockets under WBS_DRY_RUN=1, except with WBS_AES67_TTL=0: those packets never
/// leave the host (loopback only), so a dry-run test daemon can still be listened to.
fn dry_run() -> bool {
    wbs_common::dry_run() && ttl() != 0
}

pub use wbs_net::host_v4;

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

/// Every group this TAP transmits on (.1 … .6), whether running or not.
pub fn groups() -> Vec<Ipv4Addr> {
    (0..ENGINE_FLOWS.len() + LOOP_OUTS.len()).map(group_for).collect()
}

/// A receiver taking `source` would feed one of our own groups back into a loop out.
pub fn is_loop(source: Option<&str>, ours: &[Ipv4Addr]) -> bool {
    source.and_then(|s| s.trim().parse::<Ipv4Addr>().ok()).is_some_and(|g| ours.contains(&g))
}

/// Where a transmitter's audio comes from.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Source {
    /// An MXL engine ring (services::mxl).
    Engine,
    /// A receiver's fold-down ring (services::receivers).
    LoopOut,
}

/// 32-bit FNV-1a of the flow id (stable SSRC, SAP message id hash and SDP session id).
fn fnv(s: &str) -> u32 {
    s.bytes().fold(0x811c_9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x0100_0193))
}

/// SAP message id hash: the flow's hash with the version folded in, so a renamed
/// transmitter is a new announcement (odd multiplier: every version differs mod 2^16).
fn msg_hash(flow_id: &str, version: u32) -> u16 {
    ((fnv(flow_id) ^ version.wrapping_mul(0x9e37_79b1)) & 0xffff) as u16
}

/// A transmitter name as asked for: trimmed, no control characters, at most 64
/// characters. None (empty) means back to the default.
pub fn sanitize_name(raw: &str) -> Option<String> {
    let clean: String = raw.chars().filter(|c| !c.is_control()).collect();
    let name: String = clean.trim().chars().take(NAME_MAX).collect();
    let name = name.trim_end().to_string();
    (!name.is_empty()).then_some(name)
}

/// SDP (RFC 4566 + AES67 / RFC 7273 clock attributes) for one transmitter.
pub use wbs_net::refclk;

#[allow(clippy::too_many_arguments)]
pub fn sdp(
    flow_id: &str,
    name: &str,
    version: u32,
    group: Ipv4Addr,
    channels: usize,
    sample_rate: u32,
    refclk: &str,
) -> String {
    let host = host_v4();
    let sess = fnv(flow_id);
    let (port, ttl) = (port(), ttl());
    format!(
        "v=0\r\n\
         o=- {sess} {version} IN IP4 {host}\r\n\
         s={name}\r\n\
         i=Ward-Beck AES67 · MXL flow {flow_id}\r\n\
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
pub fn sap_packet(flow_id: &str, version: u32, sdp: &str, delete: bool) -> Vec<u8> {
    let mut p = Vec::with_capacity(24 + sdp.len());
    p.push(if delete { 0x24 } else { 0x20 }); // V=1, A=0, R=0, T=delete, E=0, C=0
    p.push(0); // auth length
    p.extend_from_slice(&msg_hash(flow_id, version).to_be_bytes());
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
    /// The SDP's s= (and the Bonjour instance name); label until renamed.
    name: String,
    /// SDP o= version: bumps on every rename.
    version: u32,
    /// Bonjour full name while registered.
    mdns: Option<String>,
    group: Ipv4Addr,
    channels: usize,
    sample_rate: u32,
    source: Source,
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
        sdp(&self.flow_id, &self.name, self.version, self.group, self.channels, self.sample_rate, &refclk(services::aoip().ptp_leader()))
    }

    fn json(&self) -> Value {
        let on = self.running.load(Ordering::Acquire);
        json!({
            "flow_id": self.flow_id,
            "label": self.label,
            "loop_out": self.source == Source::LoopOut,
            "name": self.name,
            "version": self.version,
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
        let mxl = services::mxl();
        let tx = |index: usize, flow_id: &str, label: String, channels: usize, sample_rate: u32, source: Source| Tx {
            flow_id: flow_id.to_string(),
            name: label.clone(),
            label,
            version: 0,
            mdns: None,
            group: group_for(index),
            channels,
            sample_rate,
            source,
            running: Arc::new(AtomicBool::new(false)),
            packets: Arc::new(AtomicU64::new(0)),
            underruns: Arc::new(AtomicU64::new(0)),
            trims: Arc::new(AtomicU64::new(0)),
            rate: Arc::new(AtomicU64::new(1.0f64.to_bits())),
            started_at: None,
            error: Arc::new(Mutex::new(None)),
            thread: None,
        };
        let mut txs: Vec<Tx> = ENGINE_FLOWS
            .iter()
            .enumerate()
            .filter_map(|(i, id)| mxl.flow_info(id).map(|f| (i, f)))
            .map(|(i, f)| tx(i, &f.flow_id, short_label(&f.label), f.channel_count, f.sample_rate, Source::Engine))
            .collect();
        // The loop outs always exist (by-id 5 / 6); they send once their receiver has audio.
        for (k, (id, label)) in LOOP_OUTS.iter().enumerate() {
            txs.push(tx(ENGINE_FLOWS.len() + k, id, label.to_string(), 2, 48000, Source::LoopOut));
        }
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
    let leader = services::aoip().ptp_leader();
    let st = state().lock().unwrap();
    json!({
        // The PTP leader the SDPs name (ts-refclk), or null: no PTP heard.
        "ptp_leader": leader.map(|(gm, domain)| json!({"identity": gm.to_uppercase().replace(':', "-"), "domain": domain})),
        "sap_group": format!("{SAP_GROUP}:{SAP_PORT}"),
        "dry_run": dry_run(),
        // The RTSP responder Bonjour points at, or null: not started yet, dry run or no port.
        "rtsp_port": RTSP.get().copied().flatten(),
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
    source: Source,
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
    let eng = services::mxl();
    let fetch = |since: i64, max: usize| match source {
        Source::Engine => eng.flow_pcm(&flow_id, since, max),
        Source::LoopOut => services::receivers().pcm(&flow_id, since, max).flatten(),
    };
    // A loop out starts unanchored (-1): its receiver may not be running yet.
    let mut cursor: i64 = match source {
        Source::Engine => fetch(-1, 256).map(|c| c.cursor).unwrap_or(0),
        Source::LoopOut => fetch(-1, 256).map(|c| c.cursor).unwrap_or(-1),
    };
    let ours = groups();
    let mut looped = false;
    let mut loop_check = Instant::now();
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

    while running.load(Ordering::Acquire) && !wbs_common::is_shutting_down() {
        // Loop outs: never send while the receiver takes one of our own groups.
        if source == Source::LoopOut && loop_check.elapsed() >= Duration::from_millis(250) {
            loop_check = Instant::now();
            let now_looped = is_loop(services::receivers().source_group(&flow_id).as_deref(), &ours);
            if now_looped != looped {
                looped = now_looped;
                let mut e = error.lock().unwrap();
                if looped {
                    *e = Some(LOOP_ERROR.into());
                    println!("[AES67] TX {flow_id}: {LOOP_ERROR}, not sending");
                } else if e.as_deref() == Some(LOOP_ERROR) {
                    *e = None;
                }
            }
        }
        // Drain whatever the ring has written since the last pass (planar f32 LE).
        let got = fetch(cursor, 4800);
        if got.is_none() && source == Source::LoopOut {
            cursor = -1; // receiver stopped: re-anchor on its next ring
        }
        if let Some(chunk) = got {
            cursor = chunk.cursor;
            nch = chunk.channels.max(1);
            let n = if source == Source::LoopOut && chunk.sample_rate != sample_rate {
                *error.lock().unwrap() = Some(format!("receiver at {} Hz: loop out sends {sample_rate} Hz only", chunk.sample_rate));
                0
            } else {
                let mut e = error.lock().unwrap();
                if e.as_deref().is_some_and(|m| m.starts_with("receiver at ")) {
                    *e = None;
                }
                chunk.frames
            };
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
            if let Some(s) = sock.as_ref().filter(|_| !looped) {
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

fn send_sap(flow_id: &str, version: u32, sdp: &str, delete: bool) {
    if dry_run() {
        return;
    }
    if let Ok(s) = multicast_socket() {
        let dest = SockAddr::from(SocketAddrV4::new(SAP_GROUP, SAP_PORT));
        let _ = s.send_to(&sap_packet(flow_id, version, sdp, delete), &dest);
    }
}

/// One announcer for all running transmitters (RFC 2974 interval, simplified to 30 s).
fn ensure_sap_thread(st: &mut State) {
    if st.sap_thread.as_ref().map(|h| !h.is_finished()).unwrap_or(false) {
        return;
    }
    st.sap_thread = Some(std::thread::spawn(|| loop {
        let live: Vec<(String, u32, String)> = {
            let st = state().lock().unwrap();
            st.txs
                .iter()
                .filter(|t| t.running.load(Ordering::Acquire))
                .map(|t| (t.flow_id.clone(), t.version, t.sdp()))
                .collect()
        };
        if live.is_empty() || wbs_common::is_shutting_down() {
            return;
        }
        for (id, v, sdp) in &live {
            send_sap(id, *v, sdp, false);
        }
        let until = Instant::now() + SAP_INTERVAL;
        while Instant::now() < until {
            std::thread::sleep(Duration::from_millis(250));
            if wbs_common::is_shutting_down() || !any_running() {
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
    t.started_at = Some(wbs_common::now_f64());
    let (id, src, g, sr) = (t.flow_id.clone(), t.source, t.group, t.sample_rate);
    let (r, p, u, tr, ra, e) =
        (t.running.clone(), t.packets.clone(), t.underruns.clone(), t.trims.clone(), t.rate.clone(), t.error.clone());
    t.thread = Some(std::thread::spawn(move || sender(id, src, g, sr, r, p, u, tr, ra, e)));
    println!("[AES67] TX {} -> {}:{} (L24/{}/{} 1 ms)", t.flow_id, t.group, port(), t.sample_rate, t.channels);
    bonjour_register(t, i);
    ensure_sap_thread(st);
}

fn stop_tx(st: &mut State, i: usize) -> bool {
    let t = &mut st.txs[i];
    let was = t.running.swap(false, Ordering::AcqRel);
    if let Some(h) = t.thread.take() {
        let _ = h.join();
    }
    t.started_at = None;
    bonjour_unregister(t);
    if was {
        send_sap(&t.flow_id, t.version, &t.sdp(), true);
        println!("[AES67] TX {} stopped", t.flow_id);
    }
    was
}

/// Renames transmitter `i` (None: back to the default). A real change bumps the version;
/// a running transmitter withdraws its old SAP announcement, announces the new one now
/// and re-registers on Bonjour.
fn rename_tx(st: &mut State, i: usize, name: Option<String>) {
    let t = &mut st.txs[i];
    let name = name.unwrap_or_else(|| t.label.clone());
    if name == t.name {
        return;
    }
    let on = t.running.load(Ordering::Acquire);
    if on {
        send_sap(&t.flow_id, t.version, &t.sdp(), true);
        bonjour_unregister(t);
    }
    println!("[AES67] TX {} renamed {:?} -> {:?}", t.flow_id, t.name, name);
    t.name = name;
    t.version = t.version.wrapping_add(1);
    if on {
        send_sap(&t.flow_id, t.version, &t.sdp(), false);
        bonjour_register(t, i);
    }
}

/// On / off a POST asks for: "enabled" if given, nothing for a rename alone (the console
/// renames on every routing change), else a toggle (a body with just flow_id).
fn wanted_on(enabled: Option<bool>, has_name: bool, running: bool) -> Option<bool> {
    match enabled {
        Some(b) => Some(b),
        None if has_name => None,
        None => Some(!running),
    }
}

/// POST /api/aes67  {"flow_id": "...", "enabled": true|false, "name": "..."}  or
/// {"all": true|false}. With neither "enabled" nor "name" the named transmitter toggles;
/// an empty (or null) name goes back to the default.
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
            let name = match body.get("name") {
                None => None,
                Some(Value::Null) => Some(None),
                Some(Value::String(s)) => Some(sanitize_name(s)),
                Some(_) => return Err("name must be a string".into()),
            };
            let on = wanted_on(body.get("enabled").and_then(|v| v.as_bool()), name.is_some(), st.txs[i].running.load(Ordering::Acquire));
            if let Some(n) = name {
                rename_tx(&mut st, i, n);
            }
            match on {
                Some(true) => start_tx(&mut st, i),
                Some(false) => {
                    stop_tx(&mut st, i);
                }
                None => {}
            }
        }
    }
    Ok(status())
}

/// Stops every transmitter, sends SAP deletions and withdraws the Bonjour records.
/// Returns the labels that were live.
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

// ---- Bonjour (RAVENNA) and the RTSP responder it points at ----

/// The mDNS responder, started with the first transmitter (None: dry run or no RTSP).
fn mdns() -> Option<&'static ServiceDaemon> {
    static D: OnceLock<Option<ServiceDaemon>> = OnceLock::new();
    D.get_or_init(|| {
        if dry_run() || rtsp_listening().is_none() {
            return None;
        }
        match ServiceDaemon::new() {
            Ok(d) => Some(d),
            Err(e) => {
                wbs_log::warn("AES67", &format!("Bonjour off: mDNS responder: {e}"));
                None
            }
        }
    })
    .as_ref()
}

static RTSP: OnceLock<Option<u16>> = OnceLock::new();

/// The RTSP responder's port, binding it on first use (None: dry run or it can't bind).
fn rtsp_listening() -> Option<u16> {
    *RTSP.get_or_init(|| {
        if dry_run() {
            return None;
        }
        let port = rtsp_port();
        match TcpListener::bind(("0.0.0.0", port)) {
            Ok(l) => {
                std::thread::spawn(move || {
                    for c in l.incoming().flatten() {
                        std::thread::spawn(move || rtsp_conn(c));
                    }
                });
                println!("[AES67] RTSP on :{port} (Bonjour {MDNS_TYPE})");
                Some(port)
            }
            Err(e) => {
                wbs_log::warn("AES67", &format!("Bonjour off: RTSP :{port}: {e}"));
                None
            }
        }
    })
}

/// This machine's Bonjour host name: "<hostname>.local.".
fn mdns_host() -> String {
    let raw = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .or_else(|| {
            std::process::Command::new("hostname").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        })
        .unwrap_or_default();
    let h: String = raw
        .trim()
        .split('.')
        .next()
        .unwrap_or("")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let h = h.trim_matches('-');
    format!("{}.local.", if h.is_empty() { "ward-beck-tap" } else { h })
}

fn bonjour_register(t: &mut Tx, i: usize) {
    let (Some(d), Some(port)) = (mdns(), rtsp_listening()) else { return };
    bonjour_unregister(t);
    let props = [("id", (i + 1).to_string()), ("flow", t.flow_id.clone())];
    match ServiceInfo::new(MDNS_TYPE, &t.name, &mdns_host(), host_v4().to_string(), port, &props[..]) {
        Ok(info) => {
            let full = info.get_fullname().to_string();
            match d.register(info) {
                Ok(()) => t.mdns = Some(full),
                Err(e) => wbs_log::warn("AES67", &format!("Bonjour {}: {e}", t.name)),
            }
        }
        Err(e) => wbs_log::warn("AES67", &format!("Bonjour {}: {e}", t.name)),
    }
}

fn bonjour_unregister(t: &mut Tx) {
    if let (Some(full), Some(d)) = (t.mdns.take(), mdns()) {
        // Wait briefly for the goodbye to go out (stop_all runs at shutdown).
        if let Ok(rx) = d.unregister(&full) {
            let _ = rx.recv_timeout(Duration::from_millis(500));
        }
    }
}

/// What an RTSP URL asks for.
#[derive(Debug, PartialEq)]
pub enum RtspTarget {
    ByName(String),
    /// 1-based, the router's MXL67-n (1 … 6).
    ById(usize),
}

#[derive(Debug, PartialEq)]
pub struct RtspRequest {
    pub method: String,
    pub url: String,
    pub cseq: Option<String>,
}

/// The request line and CSeq of one RTSP request (headers up to the blank line).
pub fn parse_rtsp_request(text: &str) -> Option<RtspRequest> {
    let mut lines = text.lines();
    let mut first = lines.next()?.split_whitespace();
    let (method, url, ver) = (first.next()?, first.next()?, first.next()?);
    if !ver.starts_with("RTSP/") {
        return None;
    }
    let cseq = lines
        .take_while(|l| !l.trim().is_empty())
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("cseq"))
        .map(|(_, v)| v.trim().to_string());
    Some(RtspRequest { method: method.to_ascii_uppercase(), url: url.to_string(), cseq })
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// rtsp://host:port/by-name/<url-encoded name> or /by-id/<n> (a bare path works too).
pub fn rtsp_target(url: &str) -> Option<RtspTarget> {
    let path = match url.split_once("://") {
        Some((_, rest)) => rest.find('/').map(|p| &rest[p..]).unwrap_or("/"),
        None => url,
    };
    let path = path.split(['?', '#']).next().unwrap_or("").trim_end_matches('/');
    if let Some(n) = path.strip_prefix("/by-name/") {
        let n = pct_decode(n);
        return (!n.is_empty()).then_some(RtspTarget::ByName(n));
    }
    let id: usize = path.strip_prefix("/by-id/")?.parse().ok()?;
    (id >= 1).then_some(RtspTarget::ById(id))
}

/// An RTSP/1.0 response; `sdp` makes it an application/sdp body.
pub fn rtsp_response(code: u16, reason: &str, cseq: Option<&str>, extra: &str, sdp: Option<&str>) -> String {
    let mut r = format!("RTSP/1.0 {code} {reason}\r\n");
    if let Some(c) = cseq {
        r += &format!("CSeq: {c}\r\n");
    }
    r += extra;
    match sdp {
        Some(b) => r += &format!("Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{b}", b.len()),
        None => r += "Content-Length: 0\r\n\r\n",
    }
    r
}

/// Answers one request; `lookup` gives the SDP of a running transmitter.
pub fn rtsp_reply(text: &str, lookup: impl Fn(&RtspTarget) -> Option<String>) -> String {
    let Some(req) = parse_rtsp_request(text) else {
        return rtsp_response(400, "Bad Request", None, "", None);
    };
    let cseq = req.cseq.as_deref();
    match req.method.as_str() {
        "OPTIONS" => rtsp_response(200, "OK", cseq, "Public: OPTIONS, DESCRIBE\r\n", None),
        "DESCRIBE" => match rtsp_target(&req.url).and_then(|t| lookup(&t)) {
            Some(sdp) => rtsp_response(200, "OK", cseq, &format!("Content-Base: {}/\r\n", req.url.trim_end_matches('/')), Some(&sdp)),
            None => rtsp_response(404, "Not Found", cseq, "", None),
        },
        _ => rtsp_response(404, "Not Found", cseq, "", None),
    }
}

/// The SDP of the running transmitter an RTSP URL names.
fn rtsp_lookup(t: &RtspTarget) -> Option<String> {
    let st = state().lock().unwrap();
    let tx = match t {
        RtspTarget::ById(n) => st.txs.get(n - 1),
        RtspTarget::ByName(name) => st.txs.iter().find(|x| &x.name == name),
    }?;
    tx.running.load(Ordering::Acquire).then(|| tx.sdp())
}

/// One RTSP client: requests in turn until it hangs up (or goes quiet for 60 s).
fn rtsp_conn(c: TcpStream) {
    let _ = c.set_read_timeout(Some(Duration::from_secs(60)));
    let Ok(mut w) = c.try_clone() else { return };
    let mut r = BufReader::new(c);
    loop {
        let mut req = String::new();
        loop {
            let mut line = String::new();
            match r.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            if line.trim().is_empty() {
                if req.is_empty() {
                    continue; // stray blank line between requests
                }
                break;
            }
            req += &line;
            if req.len() > 8192 {
                return;
            }
        }
        // Requests we answer carry no body; skip one if a client sends it anyway.
        let body = req
            .lines()
            .filter_map(|l| l.split_once(':'))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if body > 0 {
            let mut skip = vec![0u8; body.min(65536)];
            if std::io::Read::read_exact(&mut r, &mut skip).is_err() {
                return;
            }
        }
        if w.write_all(rtsp_reply(&req, rtsp_lookup).as_bytes()).is_err() {
            return;
        }
    }
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
        let p = sap_packet("f", 0, "v=0\r\n", false);
        assert_eq!(p[0], 0x20);
        assert_eq!(p[1], 0);
        assert!(p.windows(16).any(|w| w == b"application/sdp\0"));
        assert_eq!(sap_packet("f", 0, "", true)[0], 0x24);
        let parsed = wbs_aoip::parse_sap(&sap_packet("f", 0, &sdp("f", "Master", 0, Ipv4Addr::new(239, 69, 1, 1), 2, 48000, &refclk(None)), false)).unwrap();
        assert_eq!(parsed["sdp"]["multicast_ip"], "239.69.1.1");
        assert_eq!(parsed["sdp"]["port"], 5004);
    }

    #[test]
    fn labels() {
        assert_eq!(short_label("Ward-Beck Master Bus (Float32 Stereo)"), "Master Bus");
    }

    #[test]
    fn sdp_name_and_version() {
        let s = sdp("wbs-flow-ref-tone", "Studio A Tone", 3, Ipv4Addr::new(239, 69, 1, 2), 2, 48000, &refclk(None));
        let line = |k: &str| s.lines().find(|l| l.starts_with(k)).unwrap().to_string();
        assert_eq!(line("s="), "s=Studio A Tone");
        assert_eq!(line("i="), "i=Ward-Beck AES67 · MXL flow wbs-flow-ref-tone");
        let o = line("o=");
        let o: Vec<&str> = o.split(' ').collect();
        assert_eq!(o[1], fnv("wbs-flow-ref-tone").to_string());
        assert_eq!(o[2], "3");
        assert!(s.contains("a=rtpmap:96 L24/48000/2\r\n"));
    }

    #[test]
    fn sap_hash_follows_version() {
        let h = |p: Vec<u8>| u16::from_be_bytes([p[2], p[3]]);
        let v0 = h(sap_packet("f", 0, "", false));
        assert_eq!(v0, (fnv("f") & 0xffff) as u16); // version 0: as before
        let mut seen = std::collections::HashSet::new();
        for v in 0..1000 {
            assert!(seen.insert(h(sap_packet("f", v, "", false))));
        }
        assert_eq!(h(sap_packet("f", 7, "", true)), h(sap_packet("f", 7, "x", false)));
    }

    #[test]
    fn names() {
        assert_eq!(sanitize_name("  Studio A  "), Some("Studio A".into()));
        assert_eq!(sanitize_name("Evil\r\ns=Other\r\n"), Some("Evils=Other".into()));
        assert_eq!(sanitize_name("Tab\there"), Some("Tabhere".into()));
        assert_eq!(sanitize_name("   "), None);
        assert_eq!(sanitize_name("\r\n"), None);
        assert_eq!(sanitize_name(&"é".repeat(100)).unwrap().chars().count(), 64);
        assert_eq!(sanitize_name(&format!("{} x", "a".repeat(63))), Some("a".repeat(63)));
    }

    #[test]
    fn rtsp_parse() {
        let r = parse_rtsp_request("DESCRIBE rtsp://10.0.0.5:9010/by-name/Studio%20A%20Tone RTSP/1.0\r\ncseq: 7\r\nAccept: application/sdp\r\n\r\n").unwrap();
        assert_eq!(r.method, "DESCRIBE");
        assert_eq!(r.cseq.as_deref(), Some("7"));
        assert_eq!(rtsp_target(&r.url), Some(RtspTarget::ByName("Studio A Tone".into())));
        assert_eq!(rtsp_target("rtsp://h:9010/by-id/3"), Some(RtspTarget::ById(3)));
        assert_eq!(rtsp_target("/by-id/2/"), Some(RtspTarget::ById(2)));
        assert_eq!(rtsp_target("rtsp://h/by-id/0"), None);
        assert_eq!(rtsp_target("rtsp://h/other"), None);
        assert_eq!(rtsp_target("rtsp://h/by-name/50%"), Some(RtspTarget::ByName("50%".into())));
        assert_eq!(parse_rtsp_request("GET / HTTP/1.1\r\n\r\n"), None);
    }

    #[test]
    fn rtsp_answers() {
        let sdp = "v=0\r\ns=Tone\r\n";
        let look = |t: &RtspTarget| (t == &RtspTarget::ById(2)).then(|| sdp.to_string());
        let ok = rtsp_reply("DESCRIBE rtsp://h:9010/by-id/2 RTSP/1.0\r\nCSeq: 3\r\n", look);
        assert!(ok.starts_with("RTSP/1.0 200 OK\r\nCSeq: 3\r\n"));
        assert!(ok.contains("Content-Type: application/sdp\r\n"));
        assert!(ok.contains(&format!("Content-Length: {}\r\n", sdp.len())));
        assert!(ok.ends_with(&format!("\r\n\r\n{sdp}")));
        let none = rtsp_reply("DESCRIBE rtsp://h:9010/by-id/4 RTSP/1.0\r\nCSeq: 4\r\n", look);
        assert!(none.starts_with("RTSP/1.0 404 Not Found\r\nCSeq: 4\r\n"));
        let opt = rtsp_reply("OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n", look);
        assert!(opt.starts_with("RTSP/1.0 200 OK\r\nCSeq: 1\r\n") && opt.contains("Public: OPTIONS, DESCRIBE"));
        assert!(rtsp_reply("SETUP rtsp://h/by-id/2 RTSP/1.0\r\nCSeq: 5\r\n", look).starts_with("RTSP/1.0 404"));
    }

    #[test]
    fn rename_never_toggles() {
        assert_eq!(wanted_on(None, true, true), None);
        assert_eq!(wanted_on(None, true, false), None);
        assert_eq!(wanted_on(None, false, true), Some(false)); // just flow_id: toggle
        assert_eq!(wanted_on(None, false, false), Some(true));
        assert_eq!(wanted_on(Some(true), true, false), Some(true));
        assert_eq!(wanted_on(Some(false), false, true), Some(false));
    }

    #[test]
    fn engine_flows_only() {
        assert_eq!(ENGINE_FLOWS[2], "wbs-flow-ref-tone-low");
        assert_eq!(group_for(3).octets()[3], 4);
    }

    #[test]
    fn loop_outs() {
        assert_eq!(LOOP_OUTS.map(|l| l.0), ["wbs-network-ams81", "wbs-moip-ams81"]);
        let g = super::groups();
        assert_eq!(g.len(), 6);
        assert_eq!(g.iter().map(|x| x.octets()[3]).collect::<Vec<_>>(), [1, 2, 3, 4, 5, 6]);
        let ours = [Ipv4Addr::new(239, 69, 7, 1), Ipv4Addr::new(239, 69, 7, 5)];
        assert!(is_loop(Some("239.69.7.5"), &ours));
        assert!(is_loop(Some(" 239.69.7.1"), &ours));
        assert!(!is_loop(Some("239.69.7.9"), &ours));
        assert!(!is_loop(Some("not an ip"), &ours));
        assert!(!is_loop(None, &ours));
    }

    #[test]
    fn groups() {
        assert_eq!(group_for(0).octets()[0..2], [239, 69]);
        assert_eq!(group_for(2).octets()[3], 3);
    }
}
