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
    fn hw_encoder(&mut self) -> Option<Box<dyn VideoEncoder>> { Some(Box::new(Encoder)) }
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

struct Encoder;
impl VideoEncoder for Encoder {
    fn name(&self) -> &'static str { "test" }
    fn set_bitrate(&mut self, _: u32) {}
    fn set_fps(&mut self, _: f32) {}
    fn force_key(&mut self) {}
    fn encode(&mut self, _: &[u8], _: usize, _: usize, _: usize, _: usize, _: usize) -> Result<(Vec<u8>, bool), String> {
        Ok((vec![0, 0, 0, 1, 0x61], false))
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

/// Compare before/after with identical synthetic frames and real protocol/pacing.
#[test]
#[ignore]
fn active_frame_pacing_benchmark() {
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
