//! The host end of the UDP transport (plan §7.3 P2): one socket in 60000–61000, AES-128-GCM with a
//! per-session key handed over SSH, FEC per message, NACK retransmits, pacing, and a simplified
//! GCC that turns the client's arrival reports into a bitrate. The client may roam: any packet that
//! opens with the key moves the peer address there.

use proto::udp::{self, Crypto, Packet, Reassembler, ReplayWindow, Report, DIR_C2S, DIR_S2C, FLAG_RELIABLE, FLAG_VIDEO};
use proto::ClientMsg;
use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const PORTS: std::ops::Range<u16> = 60000..61000;

/// Applies the client's reliable messages (odd ids, 2n + 1) in order: a key's release must never
/// overtake its press because the press needed a resend. A gap is skipped after a second.
#[derive(Default)]
pub struct InOrder { next: u64, held: std::collections::BTreeMap<u64, Vec<u8>>, waiting_since: Option<Instant> }

impl InOrder {
    pub fn push(&mut self, msg: u32, bytes: Vec<u8>) -> Vec<Vec<u8>> {
        let n = (msg as u64).saturating_sub(1) / 2;
        if n < self.next { return Vec::new(); }
        self.held.insert(n, bytes);
        self.drain()
    }

    fn drain(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(b) = self.held.remove(&self.next) { out.push(b); self.next += 1; }
        self.waiting_since = if self.held.is_empty() { None } else { self.waiting_since.or(Some(Instant::now())) };
        out
    }

    /// After a second stuck on a gap, the held messages go out anyway.
    pub fn overdue(&mut self) -> Vec<Vec<u8>> {
        if self.waiting_since.is_none_or(|t| t.elapsed() < Duration::from_secs(1)) { return Vec::new(); }
        if let Some(&first) = self.held.keys().next() { self.next = first; }
        self.waiting_since = None;
        self.drain()
    }
}
/// Silence after which the link counts as down and video falls back to the SSH channel.
pub const ALIVE_US: u64 = 3_000_000;

fn now_us() -> u64 { crate::serve::now_us() }

/// `data`: the message's data shards. The phone stamps `last_us` when they are all in (it is whole
/// then), so they, not the parity that keeps arriving, span first_us..last_us.
struct Sent { frame_bytes: usize, last_us: u64, data: usize }

/// A reliable message awaiting RACK: its original protocol frame (type, payload) for SSH fallback
/// if retries exhaust, its encoded UDP shard packets, when they last went out, and resend count.
struct Unacked {
    ty: u8,
    payload: Vec<u8>,
    packets: Vec<Vec<u8>>,
    at: Instant,
    tries: u32,
}

/// Cap on queued legacy `SPK_AUDIO` datagrams inside the UDP pacer (~640 ms at 2x shards).
const MAX_PACED_SPK_PACKETS: usize = 64;
/// DSCP Expedited Forwarding (EF = 46 << 2 = 0xB8) for Wi-Fi WMM AC_VO / low-latency routing.
const DSCP_EF_TOS: u32 = 0xB8;

fn set_voice_dscp(sock: &UdpSocket) {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let tos = DSCP_EF_TOS as libc::c_int;
        let is_v6 = sock.local_addr().is_ok_and(|a| a.is_ipv6());
        unsafe {
            if is_v6 {
                let _ = libc::setsockopt(
                    sock.as_raw_fd(),
                    libc::IPPROTO_IPV6,
                    libc::IPV6_TCLASS,
                    (&tos as *const libc::c_int).cast(),
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            } else {
                let _ = libc::setsockopt(
                    sock.as_raw_fd(),
                    libc::IPPROTO_IP,
                    libc::IP_TOS,
                    (&tos as *const libc::c_int).cast(),
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (sock, DSCP_EF_TOS);
    }
}
/// Overhead added when sealing a plaintext packet (9-byte header + 16-byte AES-GCM tag).
const SEAL_OVERHEAD: usize = 25;

struct Pacer {
    state: Mutex<PacerState>,
    cvar: Condvar,
}

struct PacerState {
    queue: VecDeque<(bool, Vec<u8>)>, // (is_spk_audio, plaintext packet)
    spk_queued: usize,
    tokens: f64,
    last: Instant,
    closed: bool,
}

impl Pacer {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PacerState {
                queue: VecDeque::new(),
                spk_queued: 0,
                tokens: (BURST * DATAGRAM) as f64,
                last: Instant::now(),
                closed: false,
            }),
            cvar: Condvar::new(),
        })
    }

    fn clear_spk_audio(&self) {
        let mut s = self.state.lock().unwrap();
        if s.spk_queued > 0 {
            s.queue.retain(|(is_spk, _)| !*is_spk);
            s.spk_queued = 0;
        }
    }
}

/// The last [History::KEEP] shards sent, by (msg, block, index), for NACKs to find at once.
#[derive(Default)]
struct History { packets: HashMap<(u32, u8, u16), Vec<u8>>, order: VecDeque<(u32, u8, u16)> }

impl History {
    const KEEP: usize = 3000;

    fn insert(&mut self, key: (u32, u8, u16), packet: Vec<u8>) {
        if self.packets.insert(key, packet).is_none() { self.order.push_back(key); }
        while self.order.len() > Self::KEEP {
            if let Some(old) = self.order.pop_front() { self.packets.remove(&old); }
        }
    }

    fn get(&self, key: &(u32, u8, u16)) -> Option<&Vec<u8>> { self.packets.get(key) }
}

/// Datagrams the pacer lets out back to back. Most frames of a desktop (a cursor's worth of change,
/// typing) fit, so they leave at once instead of trickling out at the target bitrate; and their
/// spread on arrival is the bottleneck's rate ([Gcc::capacity]).
const BURST: usize = 16;
const DATAGRAM: usize = udp::SHARD + 40;

struct Gcc {
    target: f64,
    last_send: Option<(u64, u64)>, // (send_last, arrive_last) of the previous report
    acc: f64,
    smooth: f64,
    samples: VecDeque<(f64, f64)>,
    window: VecDeque<(u64, usize, u16, u16)>, // (time, bytes, received, total)
    /// Arrival rates of bursts (bits/s), with when they were measured.
    trains: VecDeque<(u64, f64)>,
    last_increase: Instant,
    pub loss: f64,
    pub fec: f32,
    pub overuse: bool,
}

impl Gcc {
    fn new(start: f64) -> Gcc {
        Gcc { target: start, last_send: None, acc: 0.0, smooth: 0.0, samples: VecDeque::new(), window: VecDeque::new(), trains: VecDeque::new(),
            last_increase: Instant::now(), loss: 0.0, fec: 0.1, overuse: false }
    }

    /// One video message's arrival, against when it was sent.
    fn on_report(&mut self, r: &Report, sent: &Sent, now: u64) {
        self.window.push_back((now, sent.frame_bytes, r.received, r.total));
        while self.window.front().is_some_and(|w| now - w.0 > 1_000_000) { self.window.pop_front(); }
        let (rx, tot): (u32, u32) = self.window.iter().fold((0, 0), |a, w| (a.0 + w.2 as u32, a.1 + w.3 as u32));
        self.loss = if tot == 0 { 0.0 } else { 1.0 - rx as f64 / tot as f64 }.max(0.0);
        // Parity follows loss: twice the measured loss plus a margin, 5–30%.
        self.fec = ((self.loss * 2.0 + 0.05) as f32).clamp(0.05, 0.30);
        if r.outcome == 0 { return; }
        // A burst's datagrams left together: how far apart they arrived is how fast the narrowest
        // link drains them. Bigger messages were paced, which would only measure the pacer.
        let n = sent.data;
        if n >= 4 && r.outcome == 1 && (r.total as usize) <= BURST && r.last_us > r.first_us {
            self.trains.push_back((now, ((n - 1) * DATAGRAM * 8) as f64 * 1e6 / (r.last_us - r.first_us) as f64));
        }
        while self.trains.front().is_some_and(|t| now - t.0 > 3_000_000) { self.trains.pop_front(); }
        if let Some((ps, pa)) = self.last_send {
            // Delay variation between consecutive messages: grows while a queue builds.
            let d = (r.last_us as f64 - pa as f64) - (sent.last_us as f64 - ps as f64);
            // Accumulated delay in ms, smoothed, against send time: its slope is the trend.
            self.acc += d / 1000.0;
            self.smooth = 0.9 * self.smooth + 0.1 * self.acc;
            self.samples.push_back((sent.last_us as f64 / 1000.0, self.smooth));
            while self.samples.len() > 20 { self.samples.pop_front(); }
        }
        self.last_send = Some((sent.last_us, r.last_us));
        let slope = trend(&self.samples);
        let modified = slope * self.samples.len().min(60) as f64 * 4.0;
        self.overuse = modified > 12.0;
        let received = self.window.iter().map(|w| w.1).sum::<usize>() as f64 * 8.0;
        if self.overuse {
            // A desktop seldom fills the link: what it sent in the last second (often a few hundred
            // kbps of typing) says nothing of what fits, and backing off to it starved the encoder,
            // which then dropped frames. Below half the target, step down from the target instead.
            let base = if received >= self.target * 0.5 { received } else { self.target };
            self.target = (base * 0.85).max(200_000.0).min(self.target);
            self.samples.clear();
            self.acc = 0.0;
            self.smooth = 0.0;
        } else {
            let dt = self.last_increase.elapsed().as_secs_f64().min(1.0);
            if self.loss > 0.10 {
                // Heavy loss: down by half the loss rate a second, prorated.
                self.target *= (1.0 - 0.5 * self.loss.min(0.6)).powf(dt);
            } else if modified > -12.0 {
                // Normal: up 8% a second, prorated.
                self.target *= 1.0 + 0.08 * dt;
            }
        }
        self.last_increase = Instant::now();
    }
}

impl Gcc {
    /// The bottleneck's rate (bits/s): the 75th percentile of the last 3 s of bursts, once there are
    /// three. Not the maximum: a queue that releases datagrams together reads faster than the link.
    fn capacity(&self) -> Option<f64> {
        if self.trains.len() < 3 { return None; }
        let mut r: Vec<f64> = self.trains.iter().map(|t| t.1).collect();
        r.sort_by(f64::total_cmp);
        Some(r[r.len() * 3 / 4])
    }

    /// The pacer's rate in bytes/s. Paced at the target bitrate, a frame twice the average size took
    /// two frame intervals to leave (40 ms of a 63 ms "network" on a phone); it only has to keep a
    /// big frame (a keyframe, a scrolled page) from overflowing the narrowest link's queue.
    fn pace_rate(&self) -> u64 {
        let bits = self.capacity().map_or(0.0, |c| c * 0.9).max(self.target * 2.5);
        (bits / 8.0).max(200_000.0) as u64
    }
}

fn trend(s: &VecDeque<(f64, f64)>) -> f64 {
    if s.len() < 3 { return 0.0; }
    let n = s.len() as f64;
    let mx = s.iter().map(|p| p.0).sum::<f64>() / n;
    let my = s.iter().map(|p| p.1).sum::<f64>() / n;
    let num: f64 = s.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    let den: f64 = s.iter().map(|p| (p.0 - mx).powi(2)).sum();
    if den == 0.0 { 0.0 } else { num / den }
}

pub struct UdpLink {
    pub port: u16,
    pub key: [u8; 16],
    sock: UdpSocket,
    crypto: Crypto,
    peer: Mutex<Option<SocketAddr>>,
    last_rx: AtomicU64,
    seq: AtomicU64,
    msg: AtomicU32,
    /// Recent shards' packets, for NACKs.
    history: Mutex<History>,
    sent: Mutex<HashMap<u32, Sent>>,
    /// Reliable messages' packets until acknowledged, with when they were last sent and how often.
    reliable: Mutex<HashMap<u32, Unacked>>,
    gcc: Mutex<Gcc>,
    pace: Mutex<Option<Arc<Pacer>>>,
    pace_rate: AtomicU64,
    rtt_us: AtomicU64,
    /// Test hook (ROAMTTY_DESKTOP_TEST=1 only): drop this share of datagrams both ways, ×1e6.
    pub drop_ppm: AtomicU32,
    pub enabled: AtomicBool,
    stats: Mutex<LinkStats>,
    /// Set when the session ends: both threads leave, the last Arc goes and the port is free again.
    closed: AtomicBool,
    /// The time in this end's last RTT probe (a heartbeat with no echo). Only its echo is a round
    /// trip: the phone echoes other heartbeats in its own next one, up to 250 ms later, which read as
    /// a 265 ms round trip on a 15 ms link and slowed every resend to match.
    probe_t: AtomicU64,
    #[cfg(test)]
    time_offset_us: AtomicU64,
}

#[derive(Default, Clone, Debug)]
pub struct LinkStats {
    pub sent_packets: u64, pub resent: u64, pub received_packets: u64,
    /// Client messages that arrived whole, by protocol type (input, ack, ping...), and those that did not parse.
    pub client: std::collections::BTreeMap<u8, u64>, pub undecodable: u64,
}

impl UdpLink {
    /// Binds the first free port of the mosh range; None when none is free. IPv6 only where the host
    /// has no IPv4 at all: a port whose IPv4 side is taken is skipped, because an IPv6 socket on it
    /// binds fine while the IPv4 datagrams keep going to the other socket, and the link never comes up.
    pub fn bind(start_rate: f64) -> Option<Arc<UdpLink>> {
        let offset = (std::process::id() as u16) % (PORTS.end - PORTS.start);
        let sock = (0..PORTS.end - PORTS.start)
            .map(|i| PORTS.start + (offset + i) % (PORTS.end - PORTS.start))
            .find_map(|p| match UdpSocket::bind(("0.0.0.0", p)) {
                Ok(s) => Some(s),
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => None,
                Err(_) => UdpSocket::bind(("::", p)).ok(),
            })?;
        Self::from_socket(sock, start_rate, false)
    }

    /// Voice-specific UDP bind: identical to `bind` plus DSCP EF (`0xB8`) for real-time voice QoS,
    /// leaving Remote Desktop's video UDP socket completely untouched.
    pub fn bind_voice(start_rate: f64) -> Option<Arc<UdpLink>> {
        let offset = u16::from_le_bytes(crate::session::random_bytes(2).try_into().ok()?) % (PORTS.end - PORTS.start);
        let sock = (0..PORTS.end - PORTS.start)
            .map(|i| PORTS.start + (offset + i) % (PORTS.end - PORTS.start))
            .find_map(|p| match UdpSocket::bind(("0.0.0.0", p)) {
                Ok(s) => Some(s),
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => None,
                Err(_) => UdpSocket::bind(("::", p)).ok(),
            })?;
        Self::from_socket(sock, start_rate, true)
    }

    /// Test helper: binds an ephemeral loopback port outside the 60000..61000 mosh range so
    /// parallel tests do not race with `sessions_in_one_process_reuse_ports_cleanly`.
    #[cfg(test)]
    pub fn bind_ephemeral(start_rate: f64) -> Option<Arc<UdpLink>> {
        let sock = UdpSocket::bind(("127.0.0.1", 0)).ok()?;
        Self::from_socket(sock, start_rate, false)
    }

    fn from_socket(sock: UdpSocket, start_rate: f64, voice_qos: bool) -> Option<Arc<UdpLink>> {
        if voice_qos {
            set_voice_dscp(&sock);
        }
        let key: [u8; 16] = crate::session::random_bytes(16).try_into().ok()?;
        let port = sock.local_addr().ok()?.port();
        sock.set_read_timeout(Some(Duration::from_millis(200))).ok()?;
        let link = Arc::new(UdpLink {
            port, key, crypto: Crypto::new(&key), sock, peer: Mutex::new(None), last_rx: AtomicU64::new(0),
            seq: AtomicU64::new(1), msg: AtomicU32::new(1), history: Mutex::new(History::default()),
            sent: Mutex::new(HashMap::new()), reliable: Mutex::new(HashMap::new()), gcc: Mutex::new(Gcc::new(start_rate)),
            pace: Mutex::new(None), pace_rate: AtomicU64::new((start_rate * 2.5 / 8.0) as u64), rtt_us: AtomicU64::new(0),
            drop_ppm: AtomicU32::new(0), enabled: AtomicBool::new(true), stats: Mutex::new(LinkStats::default()),
            closed: AtomicBool::new(false), probe_t: AtomicU64::new(0),
            #[cfg(test)]
            time_offset_us: AtomicU64::new(0),
        });
        Some(link)
    }

    /// The session is over. The macOS and Windows agents serve every session from one long-lived
    /// process: without this each one leaked its socket and two threads, and the next session to
    /// land on the port never heard from the phone.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        if let Some(pacer) = self.pace.lock().unwrap().take() {
            pacer.state.lock().unwrap().closed = true;
            pacer.cvar.notify_all();
        }
    }

    fn link_now_us(&self) -> u64 {
        #[cfg(test)]
        { now_us().saturating_add(self.time_offset_us.load(Ordering::Relaxed)) }
        #[cfg(not(test))]
        { now_us() }
    }

    pub fn alive(&self) -> bool {
        self.enabled.load(Ordering::Relaxed) && self.peer.lock().unwrap().is_some()
            && self.link_now_us().saturating_sub(self.last_rx.load(Ordering::Relaxed)) < ALIVE_US
    }

    pub fn ever_received(&self) -> bool { self.last_rx.load(Ordering::Relaxed) != 0 }
    pub fn silent_us(&self) -> u64 { self.link_now_us().saturating_sub(self.last_rx.load(Ordering::Relaxed)) }
    #[cfg(test)]
    pub fn advance_clock_us(&self, delta_us: u64) {
        self.time_offset_us.fetch_add(delta_us, Ordering::Relaxed);
    }
    pub fn rtt_ms(&self) -> u64 { self.rtt_us.load(Ordering::Relaxed) / 1000 }
    pub fn stats(&self) -> LinkStats { self.stats.lock().unwrap().clone() }

    /// One whole client frame to the session.
    fn deliver(&self, bytes: &[u8], tx: &Sender<Option<ClientMsg>>) {
        let parsed = proto::read_frame(&mut &bytes[..]).ok().flatten().and_then(|(t, p)| proto::decode_client(t, &p).ok().map(|m| (t, m)));
        let mut st = self.stats.lock().unwrap();
        match parsed {
            Some((t, m)) => { *st.client.entry(t).or_default() += 1; drop(st); let _ = tx.send(Some(m)); }
            None => st.undecodable += 1,
        }
    }

    /// (bitrate, fec ratio, loss, overuse) from the congestion controller.
    pub fn estimate(&self) -> (f64, f32, f64, bool) {
        let g = self.gcc.lock().unwrap();
        (g.target, g.fec, g.loss, g.overuse)
    }

    pub fn clamp_target(&self, cap: f64) {
        let mut g = self.gcc.lock().unwrap();
        g.target = g.target.clamp(200_000.0, cap);
        self.pace_rate.store(g.pace_rate(), Ordering::Relaxed);
    }

    fn dropped(&self) -> bool {
        let ppm = self.drop_ppm.load(Ordering::Relaxed);
        if ppm == 0 { return false; }
        let b: [u8; 4] = crate::session::random_bytes(4).try_into().unwrap();
        u32::from_le_bytes(b) % 1_000_000 < ppm
    }

    fn seal_and_send(&self, plain: &[u8], peer: SocketAddr) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let packet = self.crypto.seal(DIR_S2C, seq, plain);
        self.stats.lock().unwrap().sent_packets += 1;
        if self.dropped() { return; }
        let _ = self.sock.send_to(&packet, peer);
    }

    fn raw_send(&self, plain: &[u8], paced: bool, is_spk_audio: bool) {
        let Some(peer) = *self.peer.lock().unwrap() else { return };
        if paced {
            if let Some(pacer) = self.pace.lock().unwrap().clone() {
                if is_spk_audio {
                    // Normal 20ms voice chunks fit inside the token bucket burst and go straight
                    // to the socket with zero extra thread hop; giant bursts trickle through pacing.
                    let burst = (BURST * DATAGRAM) as f64;
                    let wire_len = (plain.len() + SEAL_OVERHEAD) as f64;
                    let rate = self.pace_rate.load(Ordering::Relaxed) as f64;
                    let mut s = pacer.state.lock().unwrap();
                    if s.closed { return; }
                    s.tokens = (s.tokens + s.last.elapsed().as_secs_f64() * rate).min(burst);
                    s.last = Instant::now();
                    if s.queue.is_empty() && s.tokens >= wire_len {
                        s.tokens -= wire_len;
                        drop(s);
                        self.seal_and_send(plain, peer);
                        return;
                    }
                    while s.spk_queued >= MAX_PACED_SPK_PACKETS {
                        if let Some(pos) = s.queue.iter().position(|(is_spk, _)| *is_spk) {
                            s.queue.remove(pos);
                            s.spk_queued -= 1;
                        } else {
                            break;
                        }
                    }
                    s.queue.push_back((true, plain.to_vec()));
                    s.spk_queued += 1;
                    pacer.cvar.notify_one();
                    return;
                }
                let mut s = pacer.state.lock().unwrap();
                if !s.closed {
                    s.queue.push_back((false, plain.to_vec()));
                    pacer.cvar.notify_one();
                }
                return;
            }
        }
        self.seal_and_send(plain, peer);
    }

    /// Sends a heartbeat to `addr` before the client has spoken, so a stateful firewall on the
    /// host lets its packets in (Windows Defender cannot be told to by a normal user).
    pub fn punch(&self, addr: SocketAddr) {
        for _ in 0..3 {
            let seq = self.seq.fetch_add(1, Ordering::Relaxed);
            let p = self.crypto.seal(DIR_S2C, seq, &Packet::Heartbeat { t: now_us(), echo: 0 }.encode());
            let _ = self.sock.send_to(&p, addr);
        }
    }

    /// Sends one protocol frame (type + length + payload). Video gets FEC at the current ratio and
    /// is paced; reliable frames get full parity and are resent until acknowledged.
    pub fn send_frame(&self, ty: u8, payload: &[u8]) {
        if ty == proto::ty::SPK_CLEAR {
            if let Some(pacer) = self.pace.lock().unwrap().as_ref() {
                pacer.clear_spk_audio();
            }
        }
        let mut bytes = Vec::with_capacity(4 + payload.len());
        let n = payload.len() as u32;
        bytes.extend([ty, (n >> 16) as u8, (n >> 8) as u8, n as u8]);
        bytes.extend_from_slice(payload);
        let msg = self.msg.fetch_add(1, Ordering::Relaxed);
        // A keyframe is what every later frame needs, and what a lost one costs a second to replace:
        // it gets at least 30% parity.
        let keyframe = ty == proto::ty::VIDEO && payload.get(4).is_some_and(|f| f & proto::VIDEO_KEY != 0);
        let (flags, ratio) = match ty {
            proto::ty::VIDEO if keyframe => (FLAG_VIDEO, self.gcc.lock().unwrap().fec.max(0.30)),
            proto::ty::VIDEO => (FLAG_VIDEO, self.gcc.lock().unwrap().fec),
            proto::ty::STATE
            | proto::ty::CURSOR_SHAPE
            | proto::ty::MIC_START
            | proto::ty::MIC_STOP
            | proto::ty::SPK_START
            | proto::ty::SPK_CLEAR
            | proto::ty::SPK_STOP => (FLAG_RELIABLE, 1.0),
            // Legacy PCM packet: one datagram and a resend would come too late to play: send it twice.
            proto::ty::AUDIO | proto::ty::SPK_AUDIO => (0, 1.0),
            // Opus voice frames carry temporal RED inside a single small datagram; bypass video pacing
            // and avoid back-to-back duplication so burst loss is handled in time, not wasted bandwidth.
            proto::ty::SPK_OPUS | proto::ty::VOICE_STATS_S2C => (0, 0.0),
            _ => (0, 0.0),
        };
        let video = flags & FLAG_VIDEO != 0;
        let is_spk_audio = ty == proto::ty::SPK_AUDIO;
        let paced = video || is_spk_audio;
        let reliable = flags & FLAG_RELIABLE != 0;
        let start = now_us();
        // Each shard leaves as soon as it is cut, encoded once: the data shards are on their way
        // while the parity is still being computed (~0.7 ms for a keyframe).
        let (mut packets, mut count, mut data) = (Vec::new(), 0usize, 0usize);
        udp::fragment_each(msg, flags, &bytes, ratio, |f| {
            let p = f.packet();
            self.raw_send(&p, paced, is_spk_audio);
            count += 1;
            if f.index < f.k { data += 1; }
            packets.push(((f.msg, f.block, f.index), p));
        });
        if video {
            let mut s = self.sent.lock().unwrap();
            // The pacer spreads out what does not fit its burst; the last shard leaves about that / rate later.
            let rate = self.pace_rate.load(Ordering::Relaxed).max(1);
            let spread = (count.saturating_sub(BURST) * DATAGRAM) as u64 * 1_000_000 / rate;
            s.insert(msg, Sent { frame_bytes: bytes.len(), last_us: start + spread, data });
            if s.len() > 600 { let min = *s.keys().min().unwrap(); s.remove(&min); }
        }
        if reliable {
            self.reliable.lock().unwrap().insert(
                msg,
                Unacked {
                    ty,
                    payload: payload.to_vec(),
                    packets: packets.iter().map(|p| p.1.clone()).collect(),
                    at: Instant::now(),
                    tries: 0,
                },
            );
        }
        let mut h = self.history.lock().unwrap();
        for (key, p) in packets { h.insert(key, p); }
    }

    /// Resends reliable frames nobody acknowledged: every max(2×RTT, 80 ms), five times at most.
    /// Returns `(ty, payload)` for frames that exhausted all retries (in original send order) so
    /// the caller can fall back to the SSH channel.
    pub fn tick(&self) -> Vec<(u8, Vec<u8>)> {
        let t = now_us();
        if self.peer.lock().unwrap().is_some() && t.saturating_sub(self.probe_t.load(Ordering::Relaxed)) > 500_000 {
            self.probe_t.store(t, Ordering::Relaxed);
            self.raw_send(&Packet::Heartbeat { t, echo: 0 }.encode(), false, false);
        }
        let wait = Duration::from_micros((self.rtt_us.load(Ordering::Relaxed) * 2).max(80_000));
        let mut again = Vec::new();
        let mut expired_ids = Vec::new();
        let mut expired = Vec::new();
        {
            let mut r = self.reliable.lock().unwrap();
            for (&id, u) in r.iter_mut() {
                if u.at.elapsed() < wait { continue; }
                if u.tries >= 5 {
                    expired_ids.push(id);
                    continue;
                }
                u.tries += 1;
                u.at = Instant::now();
                again.extend(u.packets.iter().cloned());
            }
            expired_ids.sort_unstable();
            for id in expired_ids {
                if let Some(u) = r.remove(&id) {
                    expired.push((u.ty, u.payload));
                }
            }
        }
        for p in again { self.raw_send(&p, false, false); self.stats.lock().unwrap().resent += 1; }
        expired
    }

    /// Starts the receiver and the pacer; client messages go to `tx` like the SSH reader's.
    pub fn start(self: &Arc<Self>, tx: Sender<Option<ClientMsg>>) {
        let pacer = Pacer::new();
        *self.pace.lock().unwrap() = Some(pacer.clone());
        let me = self.clone();
        std::thread::spawn(move || {
            // Token bucket at [Gcc::pace_rate], bursts of up to BURST datagrams.
            let burst = (BURST * DATAGRAM) as f64;
            loop {
                let plain = {
                    let mut s = pacer.state.lock().unwrap();
                    loop {
                        if me.closed.load(Ordering::Relaxed) || s.closed { return; }
                        let Some((_, front)) = s.queue.front() else {
                            s = pacer.cvar.wait(s).unwrap();
                            continue;
                        };
                        let need_bytes = (front.len() + SEAL_OVERHEAD) as f64;
                        let rate = me.pace_rate.load(Ordering::Relaxed) as f64;
                        s.tokens = (s.tokens + s.last.elapsed().as_secs_f64() * rate).min(burst);
                        s.last = Instant::now();
                        if s.tokens >= need_bytes {
                            s.tokens -= need_bytes;
                            let (is_spk, p) = s.queue.pop_front().unwrap();
                            if is_spk { s.spk_queued = s.spk_queued.saturating_sub(1); }
                            break p;
                        }
                        let wait_s = ((need_bytes - s.tokens) / rate.max(1.0)).min(0.005);
                        let (guard, _) = pacer.cvar.wait_timeout(s, Duration::from_secs_f64(wait_s)).unwrap();
                        s = guard;
                    }
                };
                if let Some(peer) = *me.peer.lock().unwrap() {
                    me.seal_and_send(&plain, peer);
                }
            }
        });
        let me = self.clone();
        std::thread::spawn(move || me.receive(tx));
    }

    fn receive(&self, tx: Sender<Option<ClientMsg>>) {
        let mut buf = vec![0u8; 2048];
        let mut replay = ReplayWindow::default();
        let mut reasm = Reassembler::default();
        let mut order = InOrder::default();
        while !self.closed.load(Ordering::Relaxed) {
            for bytes in order.overdue() { self.deliver(&bytes, &tx); }
            let (n, from) = match self.sock.recv_from(&mut buf) {
                Ok(v) => v,
                Err(_) => {
                    reasm.expire(now_us(), 2_000_000);
                    for bytes in order.overdue() { self.deliver(&bytes, &tx); }
                    continue;
                }
            };
            if self.dropped() { continue; }
            let Some((dir, seq, plain)) = self.crypto.open(&buf[..n]) else { continue };
            if dir != DIR_C2S || !replay.accept(seq) { continue; }
            // Roaming: the newest address that holds the key is where the client is now.
            {
                let mut p = self.peer.lock().unwrap();
                if *p != Some(from) {
                    crate::log!("udp peer {} → {from}", p.map(|a| a.to_string()).unwrap_or_else(|| "none".into()));
                    *p = Some(from);
                }
            }
            self.last_rx.store(self.link_now_us(), Ordering::Relaxed);
            self.stats.lock().unwrap().received_packets += 1;
            let Some(packet) = Packet::decode(&plain) else { continue };
            match packet {
                Packet::Heartbeat { t, echo } => {
                    if echo != 0 && echo == self.probe_t.load(Ordering::Relaxed) {
                        self.rtt_us.store(now_us().saturating_sub(echo), Ordering::Relaxed);
                    }
                    self.raw_send(&Packet::Heartbeat { t: now_us(), echo: t }.encode(), false, false);
                }
                Packet::Frag(f) => {
                    let reliable = f.flags & FLAG_RELIABLE != 0;
                    let msg = f.msg;
                    match reasm.push(f, now_us()) {
                        Some(d) => {
                            if reliable { self.raw_send(&Packet::Rack(vec![msg]).encode(), false, false); }
                            let ready = if reliable { order.push(msg, d.bytes) } else { vec![d.bytes] };
                            for bytes in ready { self.deliver(&bytes, &tx); }
                        }
                        // A resend of something already delivered: the RACK was lost, say it again.
                        // Not for a shard of a message still being assembled: that RACK would stop its resends.
                        None if reliable && reasm.is_done(msg) => self.raw_send(&Packet::Rack(vec![msg]).encode(), false, false),
                        None => {}
                    }
                    reasm.expire(now_us(), 2_000_000);
                    for bytes in order.overdue() { self.deliver(&bytes, &tx); }
                }
                Packet::Rack(ids) => { let mut r = self.reliable.lock().unwrap(); for i in ids { r.remove(&i); } }
                Packet::Nack { msg, block, indices } => {
                    let shards: Vec<Vec<u8>> = {
                        let h = self.history.lock().unwrap();
                        indices.iter().filter_map(|i| h.get(&(msg, block, *i)).cloned()).collect()
                    };
                    for s in shards { self.raw_send(&s, false, false); self.stats.lock().unwrap().resent += 1; }
                }
                Packet::Feedback(reports) => {
                    let now = now_us();
                    for r in reports {
                        let sent = self.sent.lock().unwrap().remove(&r.msg);
                        if let Some(s) = sent { self.gcc.lock().unwrap().on_report(&r, &s, now); }
                    }
                    let g = self.gcc.lock().unwrap();
                    self.pace_rate.store(g.pace_rate(), Ordering::Relaxed);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// One process serving session after session (the macOS and Windows agents): a closed link frees
    /// its port, and while a link is open the next one takes another port instead of an IPv6 twin.
    #[test]
    fn sessions_in_one_process_reuse_ports_cleanly() {
        let (tx, _rx) = mpsc::channel();
        let a = UdpLink::bind(1_000_000.0).unwrap();
        a.start(tx.clone());
        let b = UdpLink::bind(1_000_000.0).unwrap();
        assert_ne!(a.port, b.port);
        assert!(b.sock.local_addr().unwrap().is_ipv4(), "{:?}", b.sock.local_addr());
        let port = a.port;
        a.close();
        drop(a);
        drop(b);
        std::thread::sleep(Duration::from_millis(400));
        let c = UdpLink::bind(1_000_000.0).unwrap();
        assert_eq!(c.port, port, "the closed link let its port go");
        assert!(c.sock.local_addr().unwrap().is_ipv4());
    }

    fn report(msg: u32, last: u64, received: u16, total: u16) -> Report {
        Report { msg, received, total, first_us: last - 1000, last_us: last, outcome: 1 }
    }

    #[test]
    fn steady_arrivals_raise_the_rate() {
        let mut g = Gcc::new(2_000_000.0);
        for i in 0..40u64 {
            let s = Sent { frame_bytes: 10_000, last_us: i * 16_000 + 1000, data: 9 };
            g.on_report(&report(i as u32, 50_000 + i * 16_000 + 1000, 10, 11), &s, i * 16_000);
            g.last_increase -= Duration::from_millis(16);
        }
        assert!(!g.overuse);
        assert!(g.target > 2_000_000.0);
    }

    #[test]
    fn growing_delay_is_overuse() {
        let mut g = Gcc::new(8_000_000.0);
        for i in 0..40u64 {
            let s = Sent { frame_bytes: 10_000, last_us: i * 16_000 + 1000, data: 9 };
            // Each arrival 6 ms later than the last send gap: a queue fills.
            g.on_report(&report(i as u32, 50_000 + i * 22_000, 10, 11), &s, i * 16_000);
        }
        assert!(g.target < 8_000_000.0, "target {}", g.target);
    }

    /// Bursts that arrive 1 ms per datagram (about 10 Mbps) set the pace, well above 2.5× a low target.
    #[test]
    fn bursts_measure_the_link_and_set_the_pace() {
        let mut g = Gcc::new(600_000.0);
        assert_eq!(g.capacity(), None);
        assert_eq!(g.pace_rate(), 200_000, "2.5 × 600 kbps is under the floor");
        for i in 0..5u64 {
            let s = Sent { frame_bytes: 10 * 1150, last_us: i * 50_000, data: 10 };
            let r = Report { msg: i as u32, received: 10, total: 10, first_us: 100_000 + i * 50_000, last_us: 109_000 + i * 50_000, outcome: 1 };
            g.on_report(&r, &s, i * 50_000);
        }
        let c = g.capacity().unwrap();
        assert!((c - 9.0 * 1190.0 * 8.0 / 0.009).abs() < 1.0, "{c}");
        assert_eq!(g.pace_rate(), (c * 0.9 / 8.0) as u64);
        // A paced message (more than a burst) is not a sample.
        let r = Report { msg: 9, received: 40, total: 40, first_us: 0, last_us: 1_000_000, outcome: 1 };
        g.on_report(&r, &Sent { frame_bytes: 40 * 1150, last_us: 0, data: 36 }, 300_000);
        assert_eq!(g.trains.len(), 5);
    }

    /// Typing sends a few hundred kbps: a queue then takes the target down a step, not to that.
    #[test]
    fn a_quiet_screen_keeps_its_target_through_overuse() {
        let mut g = Gcc::new(8_000_000.0);
        for i in 0..40u64 {
            let s = Sent { frame_bytes: 1_000, last_us: i * 16_000 + 1000, data: 1 };
            g.on_report(&report(i as u32, 50_000 + i * 22_000, 2, 2), &s, i * 16_000);
        }
        assert!(g.target < 8_000_000.0 && g.target > 2_000_000.0, "target {}", g.target);
    }

    #[test]
    fn history_finds_recent_shards_and_forgets_old_ones() {
        let mut h = History::default();
        for i in 0..History::KEEP as u32 + 10 { h.insert((i, 0, 0), vec![i as u8]); }
        assert_eq!(h.get(&(9, 0, 0)), None, "the oldest went");
        assert_eq!(h.get(&(10, 0, 0)), Some(&vec![10]));
        assert_eq!(h.packets.len(), History::KEEP);
        // Sent again under the same key (a resend): one entry, the new packet.
        h.insert((10, 0, 0), vec![99]);
        assert_eq!((h.get(&(10, 0, 0)), h.order.len()), (Some(&vec![99]), History::KEEP));
    }

    #[test]
    fn reliable_messages_apply_in_order() {
        let mut o = InOrder::default();
        assert_eq!(o.push(1, vec![0]), vec![vec![0]]);
        assert!(o.push(5, vec![2]).is_empty(), "held until 3 arrives");
        assert_eq!(o.push(3, vec![1]), vec![vec![1], vec![2]]);
        assert!(o.push(3, vec![1]).is_empty(), "a resend is dropped");
        assert!(o.push(9, vec![4]).is_empty());
        o.waiting_since = Some(Instant::now() - Duration::from_secs(2));
        assert_eq!(o.overdue(), vec![vec![4]], "a lost message does not block forever");
    }

    #[test]
    fn parity_follows_loss() {
        let mut g = Gcc::new(4_000_000.0);
        let s = Sent { frame_bytes: 1000, last_us: 1, data: 1 };
        g.on_report(&report(1, 1000, 90, 100), &s, 0);
        assert!((g.fec - 0.25).abs() < 0.01, "fec {}", g.fec);
        g.on_report(&report(2, 2000, 1000, 1000), &s, 1);
        assert!(g.fec < 0.25);
    }

    #[test]
    fn reliable_tick_returns_expired_frames_in_order() {
        let (tx, _rx) = mpsc::channel();
        let link = UdpLink::bind_ephemeral(1_000_000.0).unwrap();
        link.start(tx);
        let client = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let peer: SocketAddr = ([127, 0, 0, 1], link.port).into();
        let crypto = Crypto::new(&link.key);

        // Establish peer address with a heartbeat and wait for echo.
        let hb = crypto.seal(DIR_C2S, 1, &Packet::Heartbeat { t: 42, echo: 0 }.encode());
        client.send_to(&hb, peer).unwrap();
        let mut buf = [0u8; 2048];
        let _ = client.recv_from(&mut buf).unwrap();

        // Send three reliable frames: acknowledge the first one via RACK, let the other two expire.
        let p0 = proto::spk_start_payload(1, 24_000, 1);
        let p1 = proto::spk_start_payload(2, 24_000, 1);
        let p2 = proto::spk_clear_payload(2);
        link.send_frame(proto::ty::SPK_START, &p0);
        // Acknowledge msg=1 so it is removed from `reliable`.
        let rack = crypto.seal(DIR_C2S, 2, &Packet::Rack(vec![1]).encode());
        client.send_to(&rack, peer).unwrap();
        // Wait briefly for receive loop to process the RACK.
        std::thread::sleep(Duration::from_millis(40));

        link.send_frame(proto::ty::SPK_START, &p1);
        link.send_frame(proto::ty::SPK_CLEAR, &p2);

        let mut expired = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while expired.len() < 2 && Instant::now() < deadline {
            expired.extend(link.tick());
            std::thread::sleep(Duration::from_millis(25));
        }
        link.close();
        assert_eq!(expired, vec![(proto::ty::SPK_START, p1), (proto::ty::SPK_CLEAR, p2)]);
    }

    #[test]
    fn receive_delivers_overdue_in_order_message_on_timeout() {
        let (tx, rx) = mpsc::channel();
        let link = UdpLink::bind_ephemeral(1_000_000.0).unwrap();
        link.start(tx);
        let client = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let peer: SocketAddr = ([127, 0, 0, 1], link.port).into();
        let crypto = Crypto::new(&link.key);

        let hb = crypto.seal(DIR_C2S, 1, &Packet::Heartbeat { t: 99, echo: 0 }.encode());
        client.send_to(&hb, peer).unwrap();
        let mut buf = [0u8; 2048];
        let _ = client.recv_from(&mut buf).unwrap();

        // Send reliable msg=3 (skipping msg=1!), then go completely silent so only the
        // `recv_from` timeout path can trigger `InOrder::overdue()`.
        let mut frame = Vec::new();
        let (t, p) = proto::encode_client(&ClientMsg::SpkAck { stream_id: 7, ok: true, error: String::new() });
        proto::write_frame(&mut frame, t, &p).unwrap();
        let mut seq = 2u64;
        for f in udp::fragment(3, FLAG_RELIABLE, &frame, 0.0) {
            let pkt = crypto.seal(DIR_C2S, seq, &Packet::Frag(f).encode());
            seq += 1;
            client.send_to(&pkt, peer).unwrap();
        }

        // Must NOT be delivered immediately (still waiting on gap msg=1), then delivered after ~1s timeout.
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
        let got = rx.recv_timeout(Duration::from_secs(2)).expect("overdue message must deliver on recv timeout");
        link.close();
        match got {
            Some(ClientMsg::SpkAck { stream_id, ok, .. }) => {
                assert_eq!(stream_id, 7);
                assert!(ok);
            }
            other => panic!("unexpected delivered message: {other:?}"),
        }
    }

    #[test]
    fn spk_audio_burst_pacing_and_spk_clear_flushes_queue() {
        let (tx, _rx) = mpsc::channel();
        let link = UdpLink::bind_ephemeral(200_000.0).unwrap();
        link.start(tx);
        let client = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        client.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let peer: SocketAddr = ([127, 0, 0, 1], link.port).into();
        let crypto = Crypto::new(&link.key);

        let hb = crypto.seal(DIR_C2S, 1, &Packet::Heartbeat { t: 7, echo: 0 }.encode());
        client.send_to(&hb, peer).unwrap();
        let mut buf = [0u8; 2048];
        let _ = client.recv_from(&mut buf).unwrap();

        // Send a 60-chunk burst (120 datagrams > BURST=16) followed immediately by SPK_CLEAR.
        // SPK_CLEAR must flush the queued SPK_AUDIO packets in the pacer and arrive promptly.
        let chunk = proto::spk_audio_payload(1, &[0xABu8; 960]);
        for _ in 0..60 {
            link.send_frame(proto::ty::SPK_AUDIO, &chunk);
        }
        link.send_frame(proto::ty::SPK_CLEAR, &proto::spk_clear_payload(1));

        let mut reasm = Reassembler::default();
        let mut saw_clear = false;
        let mut audio_after_clear = 0usize;
        let deadline = Instant::now() + Duration::from_millis(600);
        while Instant::now() < deadline {
            match client.recv_from(&mut buf) {
                Ok((n, _)) => {
                    let Some((DIR_S2C, _, plain)) = crypto.open(&buf[..n]) else { continue };
                    if let Some(Packet::Frag(f)) = Packet::decode(&plain) {
                        if let Some(d) = reasm.push(f, now_us()) {
                            if let Ok(Some((ty, _))) = proto::read_frame(&mut d.bytes.as_slice()) {
                                if ty == proto::ty::SPK_CLEAR {
                                    saw_clear = true;
                                } else if ty == proto::ty::SPK_AUDIO && saw_clear {
                                    audio_after_clear += 1;
                                }
                            }
                        }
                    }
                }
                Err(_) if saw_clear => break,
                Err(_) => continue,
            }
        }
        link.close();
        assert!(saw_clear, "SPK_CLEAR should arrive promptly");
        // At most a single in-flight packet could still be exiting send_to when clear ran, never dozens.
        assert!(audio_after_clear <= 2, "stale paced SPK_AUDIO leaked after SPK_CLEAR: {audio_after_clear}");
    }
}
