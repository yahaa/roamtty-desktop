//! The host end of the UDP transport (plan §7.3 P2): one socket in 60000–61000, AES-128-GCM with a
//! per-session key handed over SSH, FEC per message, NACK retransmits, pacing, and a simplified
//! GCC that turns the client's arrival reports into a bitrate. The client may roam: any packet that
//! opens with the key moves the peer address there.

use proto::udp::{self, Crypto, Packet, Reassembler, ReplayWindow, Report, DIR_C2S, DIR_S2C, FLAG_RELIABLE, FLAG_VIDEO};
use proto::ClientMsg;
use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
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

/// A reliable message's packets, when they last went out and how many times they were resent.
type Unacked = (Vec<Vec<u8>>, Instant, u32);

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
    pace: Mutex<Option<Sender<Vec<u8>>>>,
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
        let key: [u8; 16] = crate::session::random_bytes(16).try_into().ok()?;
        let offset = (std::process::id() as u16) % (PORTS.end - PORTS.start);
        let sock = (0..PORTS.end - PORTS.start)
            .map(|i| PORTS.start + (offset + i) % (PORTS.end - PORTS.start))
            .find_map(|p| match UdpSocket::bind(("0.0.0.0", p)) {
                Ok(s) => Some(s),
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => None,
                Err(_) => UdpSocket::bind(("::", p)).ok(),
            })?;
        let port = sock.local_addr().ok()?.port();
        sock.set_read_timeout(Some(Duration::from_millis(200))).ok()?;
        let link = Arc::new(UdpLink {
            port, key, crypto: Crypto::new(&key), sock, peer: Mutex::new(None), last_rx: AtomicU64::new(0),
            seq: AtomicU64::new(1), msg: AtomicU32::new(1), history: Mutex::new(History::default()),
            sent: Mutex::new(HashMap::new()), reliable: Mutex::new(HashMap::new()), gcc: Mutex::new(Gcc::new(start_rate)),
            pace: Mutex::new(None), pace_rate: AtomicU64::new((start_rate * 2.5 / 8.0) as u64), rtt_us: AtomicU64::new(0),
            drop_ppm: AtomicU32::new(0), enabled: AtomicBool::new(true), stats: Mutex::new(LinkStats::default()),
            closed: AtomicBool::new(false), probe_t: AtomicU64::new(0),
        });
        Some(link)
    }

    /// The session is over. The macOS and Windows agents serve every session from one long-lived
    /// process: without this each one leaked its socket and two threads, and the next session to
    /// land on the port never heard from the phone.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.pace.lock().unwrap().take();
    }

    pub fn alive(&self) -> bool {
        self.enabled.load(Ordering::Relaxed) && self.peer.lock().unwrap().is_some()
            && now_us().saturating_sub(self.last_rx.load(Ordering::Relaxed)) < ALIVE_US
    }

    pub fn silent_us(&self) -> u64 { now_us().saturating_sub(self.last_rx.load(Ordering::Relaxed)) }
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

    fn raw_send(&self, plain: &[u8], paced: bool) {
        let Some(peer) = *self.peer.lock().unwrap() else { return };
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let packet = self.crypto.seal(DIR_S2C, seq, plain);
        self.stats.lock().unwrap().sent_packets += 1;
        if self.dropped() { return; }
        if paced {
            if let Some(tx) = self.pace.lock().unwrap().as_ref() { let _ = tx.send(packet); return; }
        }
        let _ = self.sock.send_to(&packet, peer);
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
            proto::ty::STATE | proto::ty::CURSOR_SHAPE => (FLAG_RELIABLE, 1.0),
            // A packet is one datagram and a resend would come too late to play: send it twice.
            proto::ty::AUDIO => (0, 1.0),
            _ => (0, 0.0),
        };
        let video = flags & FLAG_VIDEO != 0;
        let reliable = flags & FLAG_RELIABLE != 0;
        let start = now_us();
        // Each shard leaves as soon as it is cut, encoded once: the data shards are on their way
        // while the parity is still being computed (~0.7 ms for a keyframe).
        let (mut packets, mut count, mut data) = (Vec::new(), 0usize, 0usize);
        udp::fragment_each(msg, flags, &bytes, ratio, |f| {
            let p = f.packet();
            self.raw_send(&p, video);
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
        if reliable { self.reliable.lock().unwrap().insert(msg, (packets.iter().map(|p| p.1.clone()).collect(), Instant::now(), 0)); }
        let mut h = self.history.lock().unwrap();
        for (key, p) in packets { h.insert(key, p); }
    }

    /// Resends reliable frames nobody acknowledged: every max(2×RTT, 80 ms), five times at most.
    pub fn tick(&self) {
        let t = now_us();
        if self.peer.lock().unwrap().is_some() && t.saturating_sub(self.probe_t.load(Ordering::Relaxed)) > 500_000 {
            self.probe_t.store(t, Ordering::Relaxed);
            self.raw_send(&Packet::Heartbeat { t, echo: 0 }.encode(), false);
        }
        let wait = Duration::from_micros((self.rtt_us.load(Ordering::Relaxed) * 2).max(80_000));
        let mut again = Vec::new();
        {
            let mut r = self.reliable.lock().unwrap();
            r.retain(|_, (packets, at, tries)| {
                if at.elapsed() < wait { return true; }
                if *tries >= 5 { return false; }
                *tries += 1;
                *at = Instant::now();
                again.extend(packets.iter().cloned());
                true
            });
        }
        for p in again { self.raw_send(&p, false); self.stats.lock().unwrap().resent += 1; }
    }

    /// Starts the receiver and the pacer; client messages go to `tx` like the SSH reader's.
    pub fn start(self: &Arc<Self>, tx: Sender<Option<ClientMsg>>) {
        let (ptx, prx) = mpsc::channel::<Vec<u8>>();
        *self.pace.lock().unwrap() = Some(ptx);
        let me = self.clone();
        std::thread::spawn(move || {
            // Token bucket at [Gcc::pace_rate], bursts of up to BURST datagrams.
            let burst = (BURST * DATAGRAM) as f64;
            let mut tokens = burst;
            let mut last = Instant::now();
            while let Ok(p) = prx.recv() {
                if me.closed.load(Ordering::Relaxed) { break; }
                loop {
                    let rate = me.pace_rate.load(Ordering::Relaxed) as f64;
                    tokens = (tokens + last.elapsed().as_secs_f64() * rate).min(burst);
                    last = Instant::now();
                    if tokens >= p.len() as f64 { tokens -= p.len() as f64; break; }
                    let need = (p.len() as f64 - tokens) / rate.max(1.0);
                    std::thread::sleep(Duration::from_secs_f64(need.min(0.005)));
                }
                if let Some(peer) = *me.peer.lock().unwrap() { let _ = me.sock.send_to(&p, peer); }
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
            let Ok((n, from)) = self.sock.recv_from(&mut buf) else {
                reasm.expire(now_us(), 2_000_000);
                continue;
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
            self.last_rx.store(now_us(), Ordering::Relaxed);
            self.stats.lock().unwrap().received_packets += 1;
            let Some(packet) = Packet::decode(&plain) else { continue };
            match packet {
                Packet::Heartbeat { t, echo } => {
                    if echo != 0 && echo == self.probe_t.load(Ordering::Relaxed) {
                        self.rtt_us.store(now_us().saturating_sub(echo), Ordering::Relaxed);
                    }
                    self.raw_send(&Packet::Heartbeat { t: now_us(), echo: t }.encode(), false);
                }
                Packet::Frag(f) => {
                    let reliable = f.flags & FLAG_RELIABLE != 0;
                    let msg = f.msg;
                    match reasm.push(f, now_us()) {
                        Some(d) => {
                            if reliable { self.raw_send(&Packet::Rack(vec![msg]).encode(), false); }
                            let ready = if reliable { order.push(msg, d.bytes) } else { vec![d.bytes] };
                            for bytes in ready { self.deliver(&bytes, &tx); }
                        }
                        // A resend of something already delivered: the RACK was lost, say it again.
                        // Not for a shard of a message still being assembled: that RACK would stop its resends.
                        None if reliable && reasm.is_done(msg) => self.raw_send(&Packet::Rack(vec![msg]).encode(), false),
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
                    for s in shards { self.raw_send(&s, false); self.stats.lock().unwrap().resent += 1; }
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
}
