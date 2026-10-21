//! `serve`: talks the protocol on stdin/stdout of the SSH channel that started it.
//! The main thread owns the screen, the encoder and stdout; a reader thread decodes stdin.

use crate::encode::{self, H264};
use crate::keymap;
use crate::session;
use crate::udp_link::UdpLink;
use crate::backend::Backend;
use proto::{ty, ClientMsg, Hello, Input, StartError};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufWriter, Write};
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Where frames go: over UDP while the link is alive, else down the SSH channel that started us.
struct Out<W: Write> {
    ssh: BufWriter<W>,
    ssh_open: bool,
    link: Option<Arc<UdpLink>>,
}

impl<W: Write> Out<W> {
    fn udp(&self) -> bool { self.link.as_ref().is_some_and(|l| l.alive()) }

    fn emit(&mut self, t: u8, payload: &[u8]) {
        if let Some(l) = self.link.as_ref().filter(|l| l.alive()) {
            l.send_frame(t, payload);
            return;
        }
        if self.ssh_open && proto::write_frame(&mut self.ssh, t, payload).is_err() { self.ssh_open = false; }
    }

    fn flush(&mut self) {
        if self.ssh_open && self.ssh.flush().is_err() { self.ssh_open = false; }
    }
}

/// The auto ladder step for a bitrate the UDP controller settled on.
fn step_for(bitrate: f64) -> usize {
    match bitrate {
        b if b >= 4_000_000.0 => 0,
        b if b >= 2_500_000.0 => 1,
        b if b >= 1_200_000.0 => 2,
        b if b >= 600_000.0 => 3,
        _ => 4,
    }
}

pub struct Args {
    pub proto: u32,
    pub client_long: u32,
    pub resume: Option<String>,
    pub token: Option<String>,
}

pub fn now_us() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts); }
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
}

fn fail(code: &str, detail: impl Into<String>) -> i32 {
    let e = StartError { error: code.into(), detail: Some(detail.into()), min: None, max: None };
    println!("{}", serde_json::to_string(&e).unwrap());
    crate::log!("serve failed: {code}");
    1
}

/// Quality caps from the plan: long edge and bitrate ceiling.
fn quality_cap(q: &str) -> (u32, u32) {
    match q {
        "smooth" => (1280, 1_000_000),
        "clear" => (1920, 4_000_000),
        "original" => (4096, 12_000_000),
        _ => (4096, 12_000_000), // auto: the rate controller decides inside these
    }
}

/// The "auto" ladder: frame rate falls first, then resolution.
const LADDER: [(u32, u32); 5] = [(1920, 60), (1920, 30), (1280, 30), (960, 15), (960, 5)];

pub struct RateControl {
    pub bitrate: u32,
    pub step: usize,
    base_rtt: u64,
    worst_queue: u64,
    last_adjust: Instant,
    stable_since: Instant,
}

impl RateControl {
    pub fn new(bitrate: u32) -> Self {
        let now = Instant::now();
        RateControl { bitrate, step: 0, base_rtt: u64::MAX, worst_queue: 0, last_adjust: now, stable_since: now }
    }

    /// `rtt_us`: time from sending a frame to its ACK. Queueing is that minus the best seen.
    pub fn on_ack(&mut self, rtt_us: u64) {
        self.base_rtt = self.base_rtt.min(rtt_us);
        self.worst_queue = self.worst_queue.max(rtt_us - self.base_rtt);
    }

    /// At most one change a second: congested → ×0.8 and one ladder step down; otherwise +5%,
    /// and one step up after 5 s without congestion. Returns true when anything changed.
    pub fn tick(&mut self, now: Instant, cap: u32, auto: bool) -> bool {
        if now.duration_since(self.last_adjust) < Duration::from_secs(1) { return false; }
        self.last_adjust = now;
        let before = (self.bitrate, self.step);
        if self.worst_queue > 50_000 {
            self.bitrate = (self.bitrate as f64 * 0.8) as u32;
            if auto && self.step + 1 < LADDER.len() { self.step += 1; }
            self.stable_since = now;
        } else {
            self.bitrate = (self.bitrate as f64 * 1.05) as u32;
            if auto && self.step > 0 && now.duration_since(self.stable_since) >= Duration::from_secs(5) {
                self.step -= 1;
                self.stable_since = now;
            }
        }
        self.bitrate = self.bitrate.clamp(200_000, cap);
        self.worst_queue = 0;
        before != (self.bitrate, self.step)
    }
}

pub fn run(args: Args) -> i32 {
    if args.proto != proto::PROTO {
        let e = StartError { error: "proto_unsupported".into(), detail: None, min: Some(proto::PROTO), max: Some(proto::PROTO) };
        println!("{}", serde_json::to_string(&e).unwrap());
        return 1;
    }
    let mut x: Box<dyn Backend> = match crate::open_desktop() {
        Ok(x) => x,
        Err((code, e)) => return fail(code, e),
    };
    let Some(lib) = encode::openh264_path().filter(|p| p.exists()) else {
        return fail("encoder_failed", "OpenH264 is not installed");
    };
    let mut enc = match H264::new(&lib) {
        Ok(e) => e,
        Err(e) => return fail("encoder_failed", e),
    };
    let displays = x.displays();
    let sess = session::claim(args.resume.as_deref(), args.token.as_deref());
    // The UDP port and a fresh key ride in the hello; the key never touches the disk.
    let link = UdpLink::bind(4_000_000.0);
    let hello = Hello {
        proto: proto::PROTO, ver: env!("CARGO_PKG_VERSION").into(), os: "linux".into(),
        session: sess.session.clone(), resume_token: sess.resume_token.clone(), displays: displays.clone(),
        enc: vec!["h264".into()], hw: "openh264".into(), needs: vec![],
        udp: link.as_ref().map(|l| proto::Udp { port: l.port, key: session::b64(&l.key) }),
    };
    let stdout = std::io::stdout();
    let mut ssh = BufWriter::with_capacity(1 << 20, stdout.lock());
    if writeln!(ssh, "{}", serde_json::to_string(&hello).unwrap()).and_then(|_| ssh.flush()).is_err() { return 1; }
    let test_hooks = std::env::var("ROAMTTY_DESKTOP_TEST").as_deref() == Ok("1");
    crate::log!("serve start {} displays={}", x.kind(), displays.len());

    let (tx, rx) = mpsc::channel::<Option<ClientMsg>>();
    if let Some(l) = &link { l.start(tx.clone()); }
    let mut out = Out { ssh, ssh_open: true, link: link.clone() };
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut r = std::io::BufReader::new(stdin.lock());
        loop {
            match proto::read_frame(&mut r) {
                Ok(Some((t, p))) => match proto::decode_client(t, &p) {
                    Ok(m) => { if tx.send(Some(m)).is_err() { return; } }
                    Err(e) => crate::log!("bad frame 0x{t:02x}: {e}"),
                },
                _ => { let _ = tx.send(None); return; }
            }
        }
    });

    let mut display = displays.iter().position(|d| d.primary).unwrap_or(0);
    let mut fps_limit = 60u32;
    let mut quality = "auto".to_string();
    let mut max_bitrate: Option<u32> = None;
    let mut lock_on_close = false;
    let mut view_only = false;
    let mut paused = false;
    let mut want_key = true;
    let mut rc = RateControl::new(4_000_000);
    enc.set_bitrate(rc.bitrate);
    let mut last_sent: Vec<u8> = Vec::new();
    let mut last_change = Instant::now();
    let mut refined = true;
    let mut frame_no: u32 = 0;
    let mut in_flight: VecDeque<(u32, u64)> = VecDeque::new();
    let mut sent_shapes: HashSet<u64> = HashSet::new();
    let mut cursor_state: Option<(u16, u16, bool, u64)> = None;
    let mut shape_hash: u64 = 0;
    let mut serial_hash: HashMap<u32, u64> = HashMap::new();
    let mut scratch = Vec::new();
    let mut next_tick = Instant::now();
    let mut ended = false;
    let mut sent_total = 0u64;
    let mut ssh_closed: Option<Instant> = None;
    let mut last_key = Instant::now() - Duration::from_secs(2);
    let mut last_udp_rate = Instant::now();
    let mut was_udp = false;

    while !ended {
        match session::RETIRED.load(Ordering::SeqCst) {
            1 => { out.emit(ty::STATE, br#"{"kicked":true}"#); out.flush(); std::thread::sleep(Duration::from_millis(300)); break; }
            2 => break,
            _ => {}
        }
        let wait = next_tick.saturating_duration_since(Instant::now());
        let msg = rx.recv_timeout(wait);
        let mut dirty_input = false;
        let mut handle = |m: ClientMsg, x: &mut Box<dyn Backend>, out: &mut Out<_>| -> Option<bool> {
            match m {
                ClientMsg::Config(c) => {
                    if let Some(d) = c.display { if (d as usize) < displays.len() && d as usize != display { display = d as usize; want_key = true; } }
                    if let Some(f) = c.fps { fps_limit = f.clamp(5, 120); }
                    if let Some(q) = c.quality { quality = q; rc.step = 0; want_key = true; }
                    if let Some(b) = c.max_bitrate { max_bitrate = Some(b); }
                    if let Some(l) = c.lock_on_close { lock_on_close = l; }
                    if let (Some(on), Some(l)) = (c.udp, &out.link) { l.enabled.store(on, Ordering::Relaxed); }
                    if let (Some(port), Some(l)) = (c.udp_client_port, &out.link) {
                        // The phone's address as sshd saw it: punch a hole towards its UDP port.
                        if let Some(ip) = std::env::var("SSH_CLIENT").ok().and_then(|v| v.split(' ').next().map(String::from))
                            .and_then(|ip| ip.parse::<std::net::IpAddr>().ok()) {
                            l.punch(std::net::SocketAddr::new(ip, port));
                        }
                    }
                    if let (Some(loss), Some(l), true) = (c.debug_loss, &out.link, test_hooks) {
                        l.drop_ppm.store((loss.clamp(0.0, 1.0) * 1_000_000.0) as u32, Ordering::Relaxed);
                        crate::log!("test hook: udp loss {loss}");
                    }
                }
                ClientMsg::Input(i) if !view_only => {
                    let d = &displays[display];
                    let r = match i {
                        Input::Abs { display: di, x: px, y: py } => {
                            let dd = displays.get(di as usize).unwrap_or(d);
                            x.move_abs(dd.x + px as i32, dd.y + py as i32)
                        }
                        Input::Rel { dx, dy } => x.move_rel(dx, dy),
                        Input::Button { button, down } => x.button(button, down),
                        Input::Wheel { dx, dy } => x.wheel(dx, dy),
                        Input::Key { hid, down } => x.key(hid, down),
                        Input::Text(s) => x.text(&s),
                    };
                    if let Err(e) = r { crate::log!("input failed: {e}"); }
                    return Some(true);
                }
                ClientMsg::Input(_) => {}
                ClientMsg::Action(a) => {
                    if !view_only {
                        let keys: &[u16] = match a {
                            proto::action::SHOW_DESKTOP => &[keymap::HID_LEFT_GUI, keymap::HID_D],
                            proto::action::ALL_WINDOWS => &[keymap::HID_LEFT_GUI],
                            _ => &[],
                        };
                        let _ = x.tap(keys);
                    }
                }
                ClientMsg::Req(r) => match r {
                    proto::req::KEYFRAME => want_key = true,
                    proto::req::PAUSE => { paused = true; in_flight.clear(); }
                    proto::req::RESUME => { paused = false; want_key = true; next_tick = Instant::now(); }
                    proto::req::VIEW_ONLY_ON => view_only = true,
                    proto::req::VIEW_ONLY_OFF => view_only = false,
                    proto::req::CLIP_GET => {
                        let text = x.clip_get().unwrap_or_default();
                        let body = serde_json::json!({ "clip": text }).to_string();
                        out.emit(ty::STATE, body.as_bytes());
                        out.flush();
                    }
                    _ => {}
                },
                ClientMsg::Ack { frame, .. } => {
                    while let Some(&(f, sent)) = in_flight.front() {
                        if f > frame { break; }
                        in_flight.pop_front();
                        if f == frame { rc.on_ack(now_us().saturating_sub(sent)); }
                    }
                }
                ClientMsg::ClipSet(s) => x.clip_set(s),
                ClientMsg::Ping(t1) => {
                    let t = now_us();
                    out.emit(ty::PONG, &proto::pong_payload(t1, t, now_us()));
                    out.flush();
                }
                ClientMsg::Pong { .. } | ClientMsg::Unknown(_) => {}
            }
            Some(false)
        };
        match msg {
            Ok(Some(m)) => {
                match handle(m, &mut x, &mut out) { None => break, Some(d) => dirty_input |= d }
                // Drain whatever else arrived so a burst of input lands in one go.
                loop {
                    match rx.try_recv() {
                        Ok(Some(m)) => match handle(m, &mut x, &mut out) { None => { ended = true; break } Some(d) => dirty_input |= d },
                        Ok(None) => { out.ssh_open = false; ssh_closed.get_or_insert_with(Instant::now); }
                        Err(_) => break,
                    }
                }
                if dirty_input { x.flush(); }
                if Instant::now() < next_tick { continue; }
            }
            // The SSH channel closed. With UDP alive the desktop carries on (plan §7.3); without
            // it, or after 30 s of UDP silence, this is the end.
            Ok(None) => { out.ssh_open = false; ssh_closed.get_or_insert_with(Instant::now); }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if ended { break; }
        if ssh_closed.is_some() && out.link.as_ref().is_none_or(|l| l.silent_us() > 30_000_000) { break; }
        if let Some(l) = &out.link { l.tick(); }
        let udp = out.udp();
        if udp != was_udp {
            crate::log!("transport {}", if udp { "udp" } else { "ssh" });
            was_udp = udp;
            in_flight.clear();
            want_key = true;
        }
        let (cap_long, cap_bitrate) = quality_cap(&quality);
        let cap_bitrate = max_bitrate.map_or(cap_bitrate, |m| m.min(cap_bitrate));
        let auto = quality == "auto";
        let (ladder_long, ladder_fps) = if auto { LADDER[rc.step] } else { (u32::MAX, u32::MAX) };
        let fps = fps_limit.min(ladder_fps).max(1);
        next_tick = Instant::now() + Duration::from_micros(1_000_000 / fps as u64);
        if paused { continue; }
        if udp {
            // GCC drives the rate on UDP. The encoder hears about it at most once a second, and
            // only for a change of 10% or more: hardware encoders stutter on constant updates.
            let l = out.link.as_ref().unwrap();
            l.clamp_target(cap_bitrate as f64);
            let (target, _, _, _) = l.estimate();
            if last_udp_rate.elapsed() >= Duration::from_secs(1) && ((target - rc.bitrate as f64).abs() / rc.bitrate as f64) >= 0.10 {
                rc.bitrate = target as u32;
                if auto { rc.step = step_for(target); }
                enc.set_bitrate(rc.bitrate);
                enc.set_fps(fps as f32);
                last_udp_rate = Instant::now();
            }
        } else if rc.tick(Instant::now(), cap_bitrate, auto) {
            enc.set_bitrate(rc.bitrate);
            enc.set_fps(fps as f32);
        }

        // Cursor first: it is cheap and keeps moving even when frames are being skipped.
        let d = displays[display].clone();
        if let Some(c) = x.cursor() {
            if let Some((hash, w, h, hx, hy, rgba)) = c.shape {
                serial_hash.insert(c.serial, hash);
                if sent_shapes.insert(hash) {
                    out.emit(ty::CURSOR_SHAPE, &proto::cursor_shape_payload(hash, w, h, hx, hy, &rgba));
                }
                shape_hash = hash;
            }
            let (cx, cy) = (c.x - d.x, c.y - d.y);
            let visible = cx >= 0 && cy >= 0 && (cx as u32) < d.w && (cy as u32) < d.h;
            let state = (cx.clamp(0, u16::MAX as i32) as u16, cy.clamp(0, u16::MAX as i32) as u16, visible, shape_hash);
            if cursor_state != Some(state) {
                out.emit(ty::CURSOR_POS, &proto::cursor_pos_payload(state.0, state.1, state.2, state.3));
                cursor_state = Some(state);
            }
        }

        // Newest frame wins: with two frames unacknowledged (four on UDP, where ACKs can be lost and
        // expire after 500 ms) this tick is skipped, not queued.
        if udp { while in_flight.front().is_some_and(|f| now_us() - f.1 > 500_000) { in_flight.pop_front(); } }
        if in_flight.len() >= if udp { 4 } else { 2 } && !want_key {
            out.flush();
            continue;
        }
        // At most one keyframe a second; a request inside that waits for it.
        let key_due = want_key && (frame_no == 0 || last_key.elapsed() >= Duration::from_secs(1));
        let capture_us = now_us();
        let frame = match x.capture(&d, &mut scratch) {
            Ok(f) => f,
            Err(e) => { crate::log!("capture failed: {e}"); continue; }
        };
        let changed = frame.bgra != last_sent.as_slice();
        let refine = !changed && !refined && last_change.elapsed() >= Duration::from_millis(300);
        if !changed && !key_due && !refine {
            out.flush();
            continue;
        }
        let long = cap_long.min(args.client_long.max(320)).min(ladder_long);
        let (w, h) = encode::fit(d.w, d.h, long);
        if key_due || refine { enc.force_key(); }
        if refine { enc.set_bitrate(cap_bitrate.max(rc.bitrate) * 3); }
        let result = enc.encode(frame.bgra, frame.w, frame.h, frame.stride, w, h);
        if refine { enc.set_bitrate(rc.bitrate); }
        if changed {
            last_sent.clear();
            last_sent.extend_from_slice(frame.bgra);
            last_change = Instant::now();
            refined = false;
        }
        let (data, key) = match result {
            Ok(r) => r,
            Err(e) => { crate::log!("encode failed: {e}"); continue; }
        };
        if data.is_empty() { continue; }
        if refine { refined = true; }
        if key { last_key = Instant::now(); want_key = false; }
        if key_due { want_key = false; }
        frame_no = frame_no.wrapping_add(1);
        let flags = (key as u8 * proto::VIDEO_KEY) | (refine as u8 * proto::VIDEO_REFINE);
        let payload = proto::video_payload(&proto::Video {
            frame: frame_no, flags, capture_us, encoded_us: now_us(), w: w as u16, h: h as u16, data: &data,
        });
        sent_total += payload.len() as u64;
        out.emit(ty::VIDEO, &payload);
        out.flush();
        if !out.ssh_open && !out.udp() && ssh_closed.is_none() { break; }
        in_flight.push_back((frame_no, now_us()));
    }
    out.flush();
    if let Some(l) = &out.link { crate::log!("udp {:?} rtt={}ms", l.stats(), l.rtt_ms()); }
    let retired = session::RETIRED.load(Ordering::SeqCst);
    crate::log!("serve end frames={frame_no} bytes={sent_total} retired={retired}");
    if retired == 0 {
        session::release();
        if lock_on_close {
            let _ = std::process::Command::new("loginctl").arg("lock-session").status();
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn congestion_backs_off_once_a_second() {
        let mut rc = RateControl::new(4_000_000);
        let t0 = Instant::now();
        rc.on_ack(20_000);
        rc.on_ack(120_000);
        assert!(!rc.tick(t0, 12_000_000, true));
        assert!(rc.tick(t0 + Duration::from_secs(1), 12_000_000, true));
        assert_eq!((rc.bitrate, rc.step), (3_200_000, 1));
        rc.on_ack(30_000);
        assert!(rc.tick(t0 + Duration::from_secs(2), 12_000_000, true));
        assert_eq!((rc.bitrate, rc.step), (3_360_000, 1));
        for s in 3..8 { rc.tick(t0 + Duration::from_secs(s), 12_000_000, true); }
        assert_eq!(rc.step, 0, "climbs back after 5 s");
    }

    #[test]
    fn manual_quality_keeps_the_ladder_and_the_cap() {
        let mut rc = RateControl::new(4_000_000);
        let t0 = Instant::now();
        rc.on_ack(10_000);
        rc.on_ack(200_000);
        rc.tick(t0 + Duration::from_secs(1), 1_000_000, false);
        assert_eq!((rc.bitrate, rc.step), (1_000_000, 0));
    }
}
