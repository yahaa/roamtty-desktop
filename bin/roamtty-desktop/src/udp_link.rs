//! The host end of the UDP transport (plan §7.3 P2): one socket in 60000–61000, AES-128-GCM with a
//! per-session key handed over SSH, FEC per message, NACK retransmits, pacing, and a simplified
//! GCC that turns the client's arrival reports into a bitrate. The client may roam: any packet that
//! opens with the key moves the peer address there.

use proto::udp::{self, Crypto, Frag, Packet, Reassembler, ReplayWindow, Report, DIR_C2S, DIR_S2C, FLAG_RELIABLE, FLAG_VIDEO};
use proto::ClientMsg;
use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const PORTS: std::ops::Range<u16> = 60000..61000;
/// Silence after which the link counts as down and video falls back to the SSH channel.
pub const ALIVE_US: u64 = 3_000_000;

fn now_us() -> u64 { crate::serve::now_us() }

struct Sent { frame_bytes: usize, last_us: u64 }

struct Gcc {
    target: f64,
    last_send: Option<(u64, u64)>, // (send_last, arrive_last) of the previous report
    acc: f64,
    smooth: f64,
    samples: VecDeque<(f64, f64)>,
    window: VecDeque<(u64, usize, u16, u16)>, // (time, bytes, received, total)
    last_increase: Instant,
    pub loss: f64,
    pub fec: f32,
    pub overuse: bool,
}

impl Gcc {
    fn new(start: f64) -> Gcc {
        Gcc { target: start, last_send: None, acc: 0.0, smooth: 0.0, samples: VecDeque::new(), window: VecDeque::new(),
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
            self.target = (received * 0.85).max(200_000.0).min(self.target);
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
    /// Recent shards by (msg, block, index), for NACKs.
    history: Mutex<VecDeque<((u32, u8, u16), Vec<u8>)>>,
    sent: Mutex<HashMap<u32, Sent>>,
    reliable: Mutex<HashMap<u32, (Vec<Frag>, Instant, u32)>>,
    gcc: Mutex<Gcc>,
    pace: Mutex<Option<Sender<Vec<u8>>>>,
    pace_rate: AtomicU64,
    rtt_us: AtomicU64,
    /// Test hook (ROAMTTY_DESKTOP_TEST=1 only): drop this share of datagrams both ways, ×1e6.
    pub drop_ppm: AtomicU32,
    pub enabled: AtomicBool,
    stats: Mutex<LinkStats>,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct LinkStats { pub sent_packets: u64, pub resent: u64, pub received_packets: u64 }

impl UdpLink {
    /// Binds the first free port of the mosh range; None when none is free.
    pub fn bind(start_rate: f64) -> Option<Arc<UdpLink>> {
        let key: [u8; 16] = crate::session::random_bytes(16).try_into().ok()?;
        let offset = (std::process::id() as u16) % (PORTS.end - PORTS.start);
        let sock = (0..PORTS.end - PORTS.start)
            .map(|i| PORTS.start + (offset + i) % (PORTS.end - PORTS.start))
            .find_map(|p| UdpSocket::bind(("0.0.0.0", p)).ok().or_else(|| UdpSocket::bind(("::", p)).ok()))?;
        let port = sock.local_addr().ok()?.port();
        sock.set_read_timeout(Some(Duration::from_millis(200))).ok()?;
        let link = Arc::new(UdpLink {
            port, key, crypto: Crypto::new(&key), sock, peer: Mutex::new(None), last_rx: AtomicU64::new(0),
            seq: AtomicU64::new(1), msg: AtomicU32::new(1), history: Mutex::new(VecDeque::new()),
            sent: Mutex::new(HashMap::new()), reliable: Mutex::new(HashMap::new()), gcc: Mutex::new(Gcc::new(start_rate)),
            pace: Mutex::new(None), pace_rate: AtomicU64::new((start_rate * 1.5 / 8.0) as u64), rtt_us: AtomicU64::new(0),
            drop_ppm: AtomicU32::new(0), enabled: AtomicBool::new(true), stats: Mutex::new(LinkStats::default()),
        });
        Some(link)
    }

    pub fn alive(&self) -> bool {
        self.enabled.load(Ordering::Relaxed) && self.peer.lock().unwrap().is_some()
            && now_us().saturating_sub(self.last_rx.load(Ordering::Relaxed)) < ALIVE_US
    }

    pub fn silent_us(&self) -> u64 { now_us().saturating_sub(self.last_rx.load(Ordering::Relaxed)) }
    pub fn rtt_ms(&self) -> u64 { self.rtt_us.load(Ordering::Relaxed) / 1000 }
    pub fn stats(&self) -> LinkStats { *self.stats.lock().unwrap() }

    /// (bitrate, fec ratio, loss, overuse) from the congestion controller.
    pub fn estimate(&self) -> (f64, f32, f64, bool) {
        let g = self.gcc.lock().unwrap();
        (g.target, g.fec, g.loss, g.overuse)
    }

    pub fn clamp_target(&self, cap: f64) {
        let mut g = self.gcc.lock().unwrap();
        g.target = g.target.clamp(200_000.0, cap);
        self.pace_rate.store((g.target * 1.5 / 8.0).max(200_000.0) as u64, Ordering::Relaxed);
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
            _ => (0, 0.0),
        };
        let frags = udp::fragment(msg, flags, &bytes, ratio);
        let video = flags & FLAG_VIDEO != 0;
        let start = now_us();
        {
            let mut h = self.history.lock().unwrap();
            for f in &frags {
                h.push_back(((f.msg, f.block, f.index), Packet::Frag(f.clone()).encode()));
            }
            while h.len() > 3000 { h.pop_front(); }
        }
        for f in &frags { self.raw_send(&Packet::Frag(f.clone()).encode(), video); }
        if video {
            let mut s = self.sent.lock().unwrap();
            // The pacer spreads the shards out; the last one leaves about bytes / rate later.
            let rate = self.pace_rate.load(Ordering::Relaxed).max(1);
            let spread = (frags.len() * (udp::SHARD + 40)) as u64 * 1_000_000 / rate;
            s.insert(msg, Sent { frame_bytes: bytes.len(), last_us: start + spread });
            if s.len() > 600 { let min = *s.keys().min().unwrap(); s.remove(&min); }
        }
        if flags & FLAG_RELIABLE != 0 { self.reliable.lock().unwrap().insert(msg, (frags, Instant::now(), 0)); }
    }

    /// Resends reliable frames nobody acknowledged: every max(2×RTT, 80 ms), five times at most.
    pub fn tick(&self) {
        let wait = Duration::from_micros((self.rtt_us.load(Ordering::Relaxed) * 2).max(80_000));
        let mut again = Vec::new();
        {
            let mut r = self.reliable.lock().unwrap();
            r.retain(|_, (frags, at, tries)| {
                if at.elapsed() < wait { return true; }
                if *tries >= 5 { return false; }
                *tries += 1;
                *at = Instant::now();
                again.extend(frags.iter().cloned());
                true
            });
        }
        for f in again { self.raw_send(&Packet::Frag(f).encode(), false); self.stats.lock().unwrap().resent += 1; }
    }

    /// Starts the receiver and the pacer; client messages go to `tx` like the SSH reader's.
    pub fn start(self: &Arc<Self>, tx: Sender<Option<ClientMsg>>) {
        let (ptx, prx) = mpsc::channel::<Vec<u8>>();
        *self.pace.lock().unwrap() = Some(ptx);
        let me = self.clone();
        std::thread::spawn(move || {
            // Token bucket at 1.5× the target bitrate, bursts of up to 8 datagrams.
            let mut tokens = 0f64;
            let mut last = Instant::now();
            while let Ok(p) = prx.recv() {
                loop {
                    let rate = me.pace_rate.load(Ordering::Relaxed) as f64;
                    tokens = (tokens + last.elapsed().as_secs_f64() * rate).min(8.0 * 1250.0);
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
        loop {
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
                    if echo != 0 { self.rtt_us.store(now_us().saturating_sub(echo), Ordering::Relaxed); }
                    self.raw_send(&Packet::Heartbeat { t: now_us(), echo: t }.encode(), false);
                }
                Packet::Frag(f) => {
                    let reliable = f.flags & FLAG_RELIABLE != 0;
                    let msg = f.msg;
                    match reasm.push(f, now_us()) {
                        Some(d) => {
                            if reliable { self.raw_send(&Packet::Rack(vec![msg]).encode(), false); }
                            if let Some(Ok(Some((t, p)))) = Some(proto::read_frame(&mut d.bytes.as_slice())) {
                                if let Ok(m) = proto::decode_client(t, &p) { let _ = tx.send(Some(m)); }
                            }
                        }
                        // A resend of something already delivered: the RACK was lost, say it again.
                        None if reliable => self.raw_send(&Packet::Rack(vec![msg]).encode(), false),
                        None => {}
                    }
                    reasm.expire(now_us(), 2_000_000);
                }
                Packet::Rack(ids) => { let mut r = self.reliable.lock().unwrap(); for i in ids { r.remove(&i); } }
                Packet::Nack { msg, block, indices } => {
                    let shards: Vec<Vec<u8>> = {
                        let h = self.history.lock().unwrap();
                        indices.iter().filter_map(|i| h.iter().find(|(k, _)| *k == (msg, block, *i)).map(|(_, p)| p.clone())).collect()
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
                    self.pace_rate.store((g.target * 1.5 / 8.0).max(200_000.0) as u64, Ordering::Relaxed);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(msg: u32, last: u64, received: u16, total: u16) -> Report {
        Report { msg, received, total, first_us: last - 1000, last_us: last, outcome: 1 }
    }

    #[test]
    fn steady_arrivals_raise_the_rate() {
        let mut g = Gcc::new(2_000_000.0);
        for i in 0..40u64 {
            let s = Sent { frame_bytes: 10_000, last_us: i * 16_000 + 1000 };
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
            let s = Sent { frame_bytes: 10_000, last_us: i * 16_000 + 1000 };
            // Each arrival 6 ms later than the last send gap: a queue fills.
            g.on_report(&report(i as u32, 50_000 + i * 22_000, 10, 11), &s, i * 16_000);
        }
        assert!(g.target < 8_000_000.0, "target {}", g.target);
    }

    #[test]
    fn parity_follows_loss() {
        let mut g = Gcc::new(4_000_000.0);
        let s = Sent { frame_bytes: 1000, last_us: 1 };
        g.on_report(&report(1, 1000, 90, 100), &s, 0);
        assert!((g.fec - 0.25).abs() < 0.01, "fec {}", g.fec);
        g.on_report(&report(2, 2000, 1000, 1000), &s, 1);
        assert!(g.fec < 0.25);
    }
}
