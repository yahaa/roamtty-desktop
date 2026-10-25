//! Exercise the real SSH protocol loop with a synthetic display/encoder and no UDP socket.
use super::*;
use crate::backend::{Cursor, Frame};
use std::io::{BufRead, BufReader};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8};
use std::sync::Mutex;

#[derive(Default)]
struct Screen {
    dirty: AtomicBool,
    wake: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    cursors: AtomicU64,
    texts: AtomicU64,
    /// What each frame encodes to, in bytes (0: a few).
    frame_bytes: std::sync::atomic::AtomicUsize,
}

impl Screen {
    fn draw(&self) {
        self.dirty.store(true, Ordering::SeqCst);
        if let Some(wake) = self.wake.lock().unwrap().as_ref() { wake(); }
    }
}

struct Desktop(Arc<Screen>, bool);
impl Backend for Desktop {
    fn kind(&self) -> &'static str { "test" }
    fn displays(&self) -> Vec<proto::Display> {
        vec![proto::Display { id: 0, w: 1920, h: 1080, scale: 1.0, x: 0, y: 0, primary: true, name: "test".into() }]
    }
    fn capture<'a>(&'a mut self, _: &proto::Display, _: &'a mut Vec<u8>) -> crate::backend::Result<Frame<'a>> {
        Ok(Frame { bgra: &[], w: 1920, h: 1080, stride: 7680,
            changed: Some(self.0.dirty.swap(false, Ordering::SeqCst)), shown_us: None })
    }
    fn hw_encoder(&mut self) -> Option<Box<dyn VideoEncoder>> { Some(Box::new(Encoder(self.0.clone()))) }
    fn on_frame(&mut self, wake: Arc<dyn Fn() + Send + Sync>) {
        if !self.1 { *self.0.wake.lock().unwrap() = Some(wake); }
    }
    fn poll_every(&self) -> Option<Duration> { self.1.then_some(Duration::from_millis(2)) }
    fn idle(&mut self) -> bool { !self.0.dirty.load(Ordering::SeqCst) }
    fn cursor(&mut self) -> Option<Cursor> { self.0.cursors.fetch_add(1, Ordering::Relaxed); None }
    fn move_abs(&mut self, _: i32, _: i32) -> crate::backend::Result<()> { Ok(()) }
    fn move_rel(&mut self, _: i16, _: i16) -> crate::backend::Result<()> { Ok(()) }
    fn button(&mut self, _: u8, _: bool) -> crate::backend::Result<()> { Ok(()) }
    fn wheel(&mut self, _: i16, _: i16) -> crate::backend::Result<()> { Ok(()) }
    fn key(&mut self, _: u16, _: bool) -> crate::backend::Result<()> { Ok(()) }
    fn text(&mut self, _: &str) -> crate::backend::Result<()> { self.0.texts.fetch_add(1, Ordering::SeqCst); Ok(()) }
    fn tap(&mut self, _: &[u16]) -> crate::backend::Result<()> { Ok(()) }
    fn flush(&mut self) {}
    fn clip_set(&mut self, _: String) {}
    fn clip_get(&mut self) -> Option<String> { None }
}

struct Encoder(Arc<Screen>);
impl VideoEncoder for Encoder {
    fn name(&self) -> &'static str { "test" }
    fn set_bitrate(&mut self, _: u32) {}
    fn set_fps(&mut self, _: f32) {}
    fn force_key(&mut self) {}
    fn encode(&mut self, _: &[u8], _: usize, _: usize, _: usize, _: usize, _: usize) -> Result<(Vec<u8>, bool), String> {
        let mut data = vec![0, 0, 0, 1, 0x61];
        data.resize(data.len().max(self.0.frame_bytes.load(Ordering::SeqCst)), 0);
        Ok((data, false))
    }
}

#[cfg(unix)]
fn thread_cpu() -> Duration {
    let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    assert_eq!(unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut t) }, 0);
    Duration::new(t.tv_sec as u64, t.tv_nsec as u32)
}

#[cfg(windows)]
fn thread_cpu() -> Duration {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let (mut created, mut exited, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe { GetThreadTimes(GetCurrentThread(), &mut created, &mut exited, &mut kernel, &mut user).unwrap(); }
    let ticks = |t: FILETIME| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
    Duration::from_nanos((ticks(kernel) + ticks(user)) * 100)
}

struct Harness {
    client: TcpStream,
    read: BufReader<TcpStream>,
    screen: Arc<Screen>,
    retired: Arc<AtomicU8>,
    worker: Option<std::thread::JoinHandle<Duration>>,
}

impl Harness {
    fn start() -> Self {
        Self::with_polling(false)
    }

    fn with_polling(polling: bool) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        client.set_nodelay(true).unwrap();
        server.set_nodelay(true).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let screen = Arc::new(Screen::default());
        screen.dirty.store(true, Ordering::SeqCst);
        let retired = Arc::new(AtomicU8::new(0));
        let args = Args { proto: proto::PROTO, client_long: 1920, resume: None, token: None,
            retired: retired.clone(), claimed: Some(session::Session { session: "test".into(), resume_token: "test".into() }) };
        let backend = screen.clone();
        let worker = std::thread::spawn(move || {
            let start = thread_cpu();
            let code = run_with(args, Box::new(server.try_clone().unwrap()), Box::new(server),
                || Ok(Box::new(Desktop(backend, polling))), |_| None);
            assert_eq!(code, 0);
            thread_cpu() - start
        });
        let mut read = BufReader::new(client.try_clone().unwrap());
        let mut hello = String::new();
        read.read_line(&mut hello).unwrap();
        assert!(serde_json::from_str::<Hello>(&hello).is_ok());
        Self { client, read, screen, retired, worker: Some(worker) }
    }

    fn send(&mut self, msg: ClientMsg) {
        let (t, p) = proto::encode_client(&msg);
        proto::write_frame(&mut self.client, t, &p).unwrap();
    }

    fn receive(&mut self, wanted: u8) -> Vec<u8> {
        loop {
            let (t, p) = proto::read_frame(&mut self.read).unwrap().expect("serve closed");
            if t == wanted { return p; }
        }
    }

    fn video(&mut self) -> u32 {
        let p = self.receive(ty::VIDEO);
        assert_eq!((u16::from_be_bytes(p[21..23].try_into().unwrap()), u16::from_be_bytes(p[23..25].try_into().unwrap())),
            (1920, 1080), "quality must not change to avoid the spin");
        u32::from_be_bytes(p[..4].try_into().unwrap())
    }

    fn ack(&mut self, frame: u32) { self.send(ClientMsg::Ack { frame, recv_us: 0, decoded_us: 0 }); }

    fn check_input(&mut self) {
        let start = Instant::now();
        self.send(ClientMsg::Input(Input::Text("test".into())));
        self.send(ClientMsg::Ping(2));
        self.receive(ty::PONG);
        assert_eq!(self.screen.texts.load(Ordering::SeqCst), 1, "input must reach the backend while frames are blocked");
        assert!(start.elapsed() < Duration::from_millis(100), "input must not wait for a frame tick");
    }

    fn stop(mut self) -> Duration {
        self.retired.store(2, Ordering::SeqCst);
        self.client.shutdown(Shutdown::Both).unwrap();
        self.worker.take().unwrap().join().unwrap()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.retired.store(2, Ordering::SeqCst);
        let _ = self.client.shutdown(Shutdown::Both);
        if let Some(worker) = self.worker.take() { let _ = worker.join(); }
    }
}

#[test]
fn paused_pending_frame_waits_and_resumes() {
    let mut h = Harness::start();
    h.video();
    h.send(ClientMsg::Req(proto::req::PAUSE));
    h.send(ClientMsg::Ping(1));
    h.receive(ty::PONG); // The loop has applied PAUSE before the next display event.
    h.screen.draw();
    std::thread::sleep(Duration::from_millis(250));
    h.check_input();
    let resume = Instant::now();
    h.send(ClientMsg::Req(proto::req::RESUME));
    h.video();
    let latency = resume.elapsed();
    let cpu = h.stop();
    println!("paused: cpu={cpu:?}, resume={latency:?}");
    assert!(cpu < Duration::from_millis(75), "paused loop burned {cpu:?} CPU in 250 ms");
    assert!(latency < Duration::from_millis(100), "resume unnecessarily waited {latency:?}");
}

#[test]
fn full_ack_window_waits_and_sends_pending_frame_on_ack() {
    full_ack_window(false);
}

#[test]
fn poll_only_backend_resumes_after_ack_without_waiting_for_tick() {
    full_ack_window(true);
}

fn full_ack_window(polling: bool) {
    let mut h = Harness::with_polling(polling);
    let mut frame = h.video();
    // A 200 ms frame tick exposes an accidental extra tick of ACK/input latency.
    proto::write_frame(&mut h.client, ty::CONFIG, br#"{"fps":5,"quality":"original"}"#).unwrap();
    for _ in 0..3 { h.screen.draw(); frame = h.video(); }
    let cursors = h.screen.cursors.load(Ordering::Relaxed);
    h.screen.draw();
    std::thread::sleep(Duration::from_millis(250));
    let loops = h.screen.cursors.load(Ordering::Relaxed) - cursors;
    h.check_input();
    let ack = Instant::now();
    h.ack(frame);
    assert_eq!(h.video(), frame + 1);
    let latency = ack.elapsed();
    let cpu = h.stop();
    println!("full window (polling={polling}): cpu={cpu:?}, ticks={loops}, ack-to-frame={latency:?}");
    // Poll-only backends keep their existing 2 ms cursor/readiness checks (about 125 here).
    assert!(loops < if polling { 500 } else { 100 }, "blocked loop spun {loops} times in 250 ms");
    assert!(cpu < Duration::from_millis(75), "blocked loop burned {cpu:?} CPU in 250 ms");
    assert!(latency < Duration::from_millis(100), "ACK unnecessarily waited {latency:?}");
}

/// An encoder that makes about five times its bitrate (VideoToolbox with a keyframe a second, OpenH264
/// on a busy screen): what goes out is held to the bitrate, and frames keep coming.
#[test]
fn an_overshooting_encoder_is_held_to_its_bitrate() {
    let mut h = Harness::start();
    let first = h.video();
    // 流畅: 1 Mbps at most; the rate control settles there within a second.
    proto::write_frame(&mut h.client, ty::CONFIG, br#"{"quality":"smooth"}"#).unwrap();
    h.screen.frame_bytes.store(20_000, Ordering::SeqCst);
    h.ack(first);
    let run = |h: &mut Harness, d: Duration| {
        let (start, mut bytes, mut frames) = (Instant::now(), 0usize, 0usize);
        while start.elapsed() < d {
            h.screen.draw();
            let p = h.receive(ty::VIDEO);
            h.ack(u32::from_be_bytes(p[..4].try_into().unwrap()));
            bytes += p.len();
            frames += 1;
        }
        (bytes as f64 * 8.0 / start.elapsed().as_secs_f64(), frames as f64 / start.elapsed().as_secs_f64())
    };
    run(&mut h, Duration::from_millis(1500));
    let (bps, fps) = run(&mut h, Duration::from_secs(3));
    h.stop();
    println!("overshooting encoder: {:.2} Mbps at {fps:.1} fps for a 1 Mbps budget", bps / 1e6);
    assert!(bps <= 1_250_000.0, "{:.2} Mbps went out for a 1 Mbps budget", bps / 1e6);
    assert!(fps >= 3.0, "frames stopped: {fps:.1} fps");
}

/// The real VideoToolbox through the real loop: scrolling noise at 2772×1800 (原画 on this Mac), a
/// keyframe asked for every second. Alone it made 24 Mbps of a 12 Mbps target. On a Mac:
/// cargo test --release vt_through_serve -- --ignored --nocapture
#[cfg(target_os = "macos")]
#[test]
#[ignore]
fn vt_through_serve_holds_original_to_its_bitrate() {
    const W: usize = 2772;
    const H: usize = 1800;
    struct Noisy { noise: Vec<u8>, i: usize }
    impl Backend for Noisy {
        fn kind(&self) -> &'static str { "test" }
        fn displays(&self) -> Vec<proto::Display> {
            vec![proto::Display { id: 0, w: W as u32, h: H as u32, scale: 1.0, x: 0, y: 0, primary: true, name: "test".into() }]
        }
        fn capture<'a>(&'a mut self, _: &proto::Display, _: &'a mut Vec<u8>) -> crate::backend::Result<Frame<'a>> {
            self.i += 1;
            let off = (self.i * 12 % H) * W * 4;
            Ok(Frame { bgra: &self.noise[off..off + W * 4 * H], w: W, h: H, stride: W * 4, changed: Some(true), shown_us: None })
        }
        fn hw_encoder(&mut self) -> Option<Box<dyn VideoEncoder>> { Some(Box::new(crate::mac::VtEncoder::new())) }
        fn cursor(&mut self) -> Option<Cursor> { None }
        fn move_abs(&mut self, _: i32, _: i32) -> crate::backend::Result<()> { Ok(()) }
        fn move_rel(&mut self, _: i16, _: i16) -> crate::backend::Result<()> { Ok(()) }
        fn button(&mut self, _: u8, _: bool) -> crate::backend::Result<()> { Ok(()) }
        fn wheel(&mut self, _: i16, _: i16) -> crate::backend::Result<()> { Ok(()) }
        fn key(&mut self, _: u16, _: bool) -> crate::backend::Result<()> { Ok(()) }
        fn text(&mut self, _: &str) -> crate::backend::Result<()> { Ok(()) }
        fn tap(&mut self, _: &[u16]) -> crate::backend::Result<()> { Ok(()) }
        fn flush(&mut self) {}
        fn clip_set(&mut self, _: String) {}
        fn clip_get(&mut self) -> Option<String> { None }
    }
    let mut noise = vec![0u8; W * 4 * H * 2];
    let mut x = 0x2545F491u32;
    for b in noise.iter_mut() { x ^= x << 13; x ^= x >> 17; x ^= x << 5; *b = x as u8; }
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    let args = Args { proto: proto::PROTO, client_long: W as u32, resume: None, token: None,
        retired: Arc::new(AtomicU8::new(0)), claimed: Some(session::Session { session: "test".into(), resume_token: "test".into() }) };
    let retired = args.retired.clone();
    let worker = std::thread::spawn(move || run_with(args, Box::new(server.try_clone().unwrap()), Box::new(server),
        move || Ok(Box::new(Noisy { noise, i: 0 })), |_| None));
    let mut read = BufReader::new(client.try_clone().unwrap());
    let mut hello = String::new();
    read.read_line(&mut hello).unwrap();
    proto::write_frame(&mut client, ty::CONFIG, br#"{"quality":"original","fps":30}"#).unwrap();
    let mut run = |d: Duration| {
        let (start, mut bytes, mut frames, mut asked) = (Instant::now(), 0usize, 0usize, Instant::now());
        while start.elapsed() < d {
            if asked.elapsed() >= Duration::from_secs(1) {
                let (t, p) = proto::encode_client(&ClientMsg::Req(proto::req::KEYFRAME));
                proto::write_frame(&mut client, t, &p).unwrap();
                asked = Instant::now();
            }
            let (t, p) = proto::read_frame(&mut read).unwrap().expect("serve closed");
            if t != ty::VIDEO { continue; }
            let (at, ap) = proto::encode_client(&ClientMsg::Ack { frame: u32::from_be_bytes(p[..4].try_into().unwrap()), recv_us: 0, decoded_us: 0 });
            proto::write_frame(&mut client, at, &ap).unwrap();
            bytes += p.len();
            frames += 1;
        }
        (bytes as f64 * 8.0 / start.elapsed().as_secs_f64(), frames as f64 / start.elapsed().as_secs_f64())
    };
    run(Duration::from_millis(1500));
    let (bps, fps) = run(Duration::from_secs(6));
    retired.store(2, Ordering::SeqCst);
    let _ = client.shutdown(Shutdown::Both);
    let _ = worker.join();
    println!("videotoolbox through serve, 原画: {:.1} Mbps at {fps:.1} fps (budget 12 Mbps)", bps / 1e6);
    assert!(bps <= 12_000_000.0 * 1.25, "{:.1} Mbps went out", bps / 1e6);
}

/// Compare before/after with identical synthetic frames and real protocol/pacing.
#[test]
#[ignore]
fn active_frame_pacing_benchmark() {
    // Measure 8 ms frame slots with a finer wait timer on headless Windows runners. Keep this
    // scoped to the pacing benchmark: CPU/input regressions and the application use normal timers.
    #[cfg(windows)]
    let _timer = {
        struct Timer;
        impl Drop for Timer {
            fn drop(&mut self) { unsafe { windows::Win32::Media::timeEndPeriod(1); } }
        }
        assert_eq!(unsafe { windows::Win32::Media::timeBeginPeriod(1) }, 0);
        Timer
    };
    for fps in [30, 60, 120] {
        let mut h = Harness::start();
        let mut frame = h.video();
        proto::write_frame(&mut h.client, ty::CONFIG, format!(r#"{{"fps":{fps},"quality":"original"}}"#).as_bytes()).unwrap();
        for _ in 0..3 { h.ack(frame); h.screen.draw(); frame = h.video(); }
        let start = Instant::now();
        for _ in 0..fps { h.ack(frame); h.screen.draw(); frame = h.video(); }
        let elapsed = start.elapsed();
        let cpu = h.stop();
        let actual = fps as f64 / elapsed.as_secs_f64();
        println!("requested={fps}fps actual={actual:.2}fps elapsed={elapsed:?} serve_cpu={cpu:?}");
        assert!(actual >= fps as f64 * 0.85 && actual <= fps as f64 * 1.05);
    }
}
