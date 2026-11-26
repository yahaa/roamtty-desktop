//! Unit, TCP loopback, UDP transport, and CLI subprocess integration tests for `voice`.

use super::*;
use proto::udp::{self, Crypto, Packet, Reassembler, DIR_C2S, DIR_S2C, FLAG_RELIABLE};
use std::net::UdpSocket;

// ============================================================================
// Pure unit tests
// ============================================================================

#[test]
fn parse_speaker_header_valid_and_invalid() {
    assert_eq!(parse_speaker_header("JETSKI-AUDIO/1 speaker rate=24000\n"), Ok(24000));
    assert_eq!(parse_speaker_header("JETSKI-AUDIO/1 speaker rate=16000\r\n"), Ok(16000));
    assert_eq!(parse_speaker_header("JETSKI-AUDIO/1 speaker rate=48000 channels=1 extra=1\n"), Ok(48000));
    assert_eq!(parse_speaker_header("JETSKI-AUDIO/1 speaker rate=8000\n"), Ok(8000));
    assert_eq!(parse_speaker_header("JETSKI-AUDIO/1 speaker rate=192000\n"), Ok(192000));

    assert!(parse_speaker_header("JETSKI-AUDIO/2 speaker rate=24000\n").is_err());
    assert!(parse_speaker_header("JETSKI-AUDIO/1 mic rate=16000\n").is_err());
    assert!(parse_speaker_header("JETSKI-AUDIO/1 speaker\n").is_err());
    assert!(parse_speaker_header("JETSKI-AUDIO/1 speaker rate=0\n").is_err());
    assert!(parse_speaker_header("JETSKI-AUDIO/1 speaker rate=7999\n").is_err());
    assert!(parse_speaker_header("JETSKI-AUDIO/1 speaker rate=192001\n").is_err());
    assert!(parse_speaker_header("JETSKI-AUDIO/1 speaker rate=abc\n").is_err());
    assert!(parse_speaker_header("JETSKI-AUDIO/1 speaker rate=24000 rate=16000\n").is_err());
    assert!(parse_speaker_header("JETSKI-AUDIO/1 speaker rate=24000 channels=2\n").is_err());
    assert!(parse_speaker_header("JETSKI-AUDIO/1 speaker rate=24000 channels=0\n").is_err());
    assert!(parse_speaker_header("JETSKI-AUDIO/1 speaker rate=24000 channels=abc\n").is_err());
    assert!(parse_speaker_header("JETSKI-AUDIO/1 speaker rate=24000 channels=1 channels=1\n").is_err());
}

#[test]
fn sanitize_err_reason_handles_newlines_and_utf8_truncation() {
    assert_eq!(sanitize_err_reason(""), "speaker error");
    assert_eq!(sanitize_err_reason("  \r\n  "), "speaker error");
    assert_eq!(sanitize_err_reason("audio\r\nbusy\nnow"), "audio  busy now");

    // 100 CJK chars = 300 bytes -> truncated safely on a 3-byte char boundary <= 235 bytes.
    let long_utf8: String = "音".repeat(100);
    let out = sanitize_err_reason(&long_utf8);
    assert!(out.len() <= SPK_ERR_MAX_BYTES, "len={}", out.len());
    assert_eq!(out.len(), 234); // 78 * 3 = 234 <= 235
    assert_eq!(out, "音".repeat(78));

    let formatted = format_spk_err("line1\nline2");
    assert_eq!(formatted, b"\nJETSKI-AUDIO/1 ERR line1 line2\n");
    assert_eq!(format_spk_ok(), b"\nJETSKI-AUDIO/1 OK\n");
}

#[test]
fn classify_peek_bytes_cases() {
    assert_eq!(classify_peek_bytes(b"JETSKI-AUDIO/1 speaker rate=24000\n"), PeekClass::Speaker);
    assert_eq!(classify_peek_bytes(b"JETSKI-AUDIO/1 "), PeekClass::Speaker);
    assert_eq!(classify_peek_bytes(b"JET"), PeekClass::Speaker);
    assert_eq!(classify_peek_bytes(b"J"), PeekClass::Speaker);
    assert_eq!(classify_peek_bytes(b"GET / HTTP/1.1"), PeekClass::Invalid);
    assert_eq!(classify_peek_bytes(b"JETTY"), PeekClass::Invalid);
    assert_eq!(classify_peek_bytes(b""), PeekClass::Eof);
}

#[test]
fn bind_mic_listener_falls_back_when_port_busy() {
    let blocker = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let busy_port = blocker.local_addr().unwrap().port();
    let (_listener, actual_port) = bind_mic_listener(busy_port).unwrap();
    assert_ne!(actual_port, 0);
    assert_ne!(actual_port, busy_port, "should fall back to an ephemeral port when requested port is busy");
}

// ============================================================================
// In-process VoiceEngine harness & integration tests
// ============================================================================

struct VoiceHarness {
    mic_port: u16,
    ev_tx: mpsc::SyncSender<Event>,
    shutdown: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    mic_accept: Option<std::thread::JoinHandle<()>>,
    last_udp: Arc<Mutex<Option<Arc<UdpLink>>>>,
}

struct ClientConn {
    stream: TcpStream,
    read: BufReader<TcpStream>,
    hello: VoiceHello,
}

impl ClientConn {
    fn send(&mut self, msg: ClientMsg) {
        let (t, p) = proto::encode_client(&msg);
        proto::write_frame(&mut self.stream, t, &p).unwrap();
        self.stream.flush().unwrap();
    }

    fn recv_frame(&mut self) -> (u8, Vec<u8>) {
        proto::read_frame(&mut self.read).unwrap().expect("voice stream closed")
    }

    fn try_recv_frame(&mut self, timeout: Duration) -> Option<(u8, Vec<u8>)> {
        let prev = self.read.get_ref().read_timeout().unwrap();
        self.read.get_ref().set_read_timeout(Some(timeout)).unwrap();
        let res = match proto::read_frame(&mut self.read) {
            Ok(Some(f)) => Some(f),
            Ok(None) => None,
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => None,
            Err(e) => panic!("unexpected read error: {e}"),
        };
        let _ = self.read.get_ref().set_read_timeout(prev);
        res
    }

    fn close(self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

impl VoiceHarness {
    fn start(enable_udp: bool) -> Self {
        Self::start_with_idle(enable_udp, Duration::from_secs(60))
    }

    fn start_with_idle(enable_udp: bool, idle_timeout: Duration) -> Self {
        let (mic_listener, mic_port) = bind_mic_listener(0).unwrap();
        let (ev_tx, ev_rx) = mpsc::sync_channel::<Event>(EV_CHANNEL_CAPACITY);
        let shutdown = Arc::new(AtomicBool::new(false));
        let client_connected = Arc::new(AtomicBool::new(false));
        let next_stream_id = Arc::new(AtomicU32::new(1));
        let mic_accept = spawn_mic_accept_loop(
            mic_listener,
            next_stream_id,
            ev_tx.clone(),
            client_connected.clone(),
            shutdown.clone(),
        );

        let sess = Session {
            session: "testvoice1234567".into(),
            resume_token: "test-resume-token-secret".into(),
        };
        let last_udp: Arc<Mutex<Option<Arc<UdpLink>>>> = Arc::new(Mutex::new(None));
        let udp_capture = last_udp.clone();
        let bind_udp: UdpBinder = if enable_udp {
            Box::new(move |rate| {
                let link = UdpLink::bind_ephemeral(rate);
                *udp_capture.lock().unwrap() = link.clone();
                link
            })
        } else {
            Box::new(|_| None)
        };
        let mut engine = VoiceEngine::new(
            mic_port,
            sess,
            idle_timeout,
            bind_udp,
            ev_tx.clone(),
            client_connected,
            shutdown.clone(),
        );
        let worker = std::thread::spawn(move || {
            let exit = engine.run(ev_rx);
            if let Some(c) = exit.active_client {
                c.close();
            }
            if let Some((reply_tx, _)) = exit.evict_reply {
                let _ = reply_tx.send(true);
            }
        });
        Self {
            mic_port,
            ev_tx,
            shutdown,
            worker: Some(worker),
            mic_accept: Some(mic_accept),
            last_udp,
        }
    }

    fn attach(&self, resume: Option<&str>, token: Option<&str>) -> Result<ClientConn, ()> {
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let client = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (server, _) = l.accept().unwrap();
        client.set_nodelay(true).unwrap();
        server.set_nodelay(true).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        server.set_write_timeout(Some(SSH_WRITE_TIMEOUT)).unwrap();

        let server_in = server.try_clone().unwrap();
        let server_close = server.try_clone().unwrap();
        let (reply_tx, reply_rx) = mpsc::channel::<bool>();
        let req = AttachReq {
            ipc_token: String::new(),
            proto: proto::PROTO,
            resume: resume.map(String::from),
            token: token.map(String::from),
            idle_timeout: None,
            ssh_client: Some("127.0.0.1 12345 22".into()),
            evict_idle_port: None,
            shutdown: false,
        };
        self.ev_tx
            .send(Event::ClientAttach {
                req,
                input: Box::new(server_in),
                output: Box::new(server),
                closer: Box::new(move || { let _ = server_close.shutdown(Shutdown::Both); }),
                reply_tx,
            })
            .unwrap();
        if !reply_rx.recv_timeout(Duration::from_secs(2)).unwrap_or(false) {
            return Err(());
        }
        let mut read = BufReader::new(client.try_clone().unwrap());
        let mut line = String::new();
        read.read_line(&mut line).unwrap();
        let hello: VoiceHello = serde_json::from_str(&line).expect("valid VoiceHello JSON");
        Ok(ClientConn { stream: client, read, hello })
    }

    fn connect_agy(&self) -> TcpStream {
        let s = TcpStream::connect(("127.0.0.1", self.mic_port)).unwrap();
        s.set_nodelay(true).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        s.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
        s
    }

    fn shutdown_and_join(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.ev_tx.send(Event::Shutdown);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        if let Some(m) = self.mic_accept.take() {
            let _ = m.join();
        }
    }

    fn stop(mut self) {
        self.shutdown_and_join();
    }
}

impl Drop for VoiceHarness {
    fn drop(&mut self) {
        self.shutdown_and_join();
    }
}

fn send_jetski_frame(s: &mut TcpStream, kind: u8, payload: &[u8]) {
    let mut hdr = [0u8; 5];
    hdr[0] = kind;
    hdr[1..5].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    s.write_all(&hdr).unwrap();
    if !payload.is_empty() {
        s.write_all(payload).unwrap();
    }
    s.flush().unwrap();
}

#[test]
fn speaker_handshake_ok_audio_barge_in_clear_and_eof() {
    let h = VoiceHarness::start(false);
    let mut client = h.attach(None, None).unwrap();
    assert_eq!(client.hello.mode, "voice");
    assert_eq!(client.hello.mic_port, h.mic_port);
    assert_eq!(client.hello.codecs, vec!["pcm16"]);

    let mut agy = h.connect_agy();
    agy.write_all(b"JETSKI-AUDIO/1 speaker rate=24000\n").unwrap();
    agy.flush().unwrap();

    // Must immediately receive SPK_START, NEVER MIC_START.
    let (t, p) = client.recv_frame();
    assert_eq!(t, ty::SPK_START);
    assert_eq!(p.len(), 9);
    let stream_id = u32::from_le_bytes(p[0..4].try_into().unwrap());
    let rate = u32::from_le_bytes(p[4..8].try_into().unwrap());
    let ch = p[8];
    assert!(stream_id >= 1);
    assert_eq!(rate, 24_000);
    assert_eq!(ch, 1);

    // Send SPK_ACK(ok=true) -> agy must get exact "\nJETSKI-AUDIO/1 OK\n" (with leading \n!).
    client.send(ClientMsg::SpkAck { stream_id, ok: true, error: String::new() });
    let mut resp = [0u8; 19];
    agy.read_exact(&mut resp).unwrap();
    assert_eq!(&resp, b"\nJETSKI-AUDIO/1 OK\n");

    // Send large 'A' PCM payload (2400 bytes -> chunked <= 960B on 2B sample boundary).
    let pcm1: Vec<u8> = (0..2400usize).map(|i| (i % 251) as u8).collect();
    send_jetski_frame(&mut agy, b'A', &pcm1);

    let mut received = Vec::new();
    while received.len() < pcm1.len() {
        let (t, p) = client.recv_frame();
        assert_eq!(t, ty::SPK_AUDIO);
        assert_eq!(u32::from_le_bytes(p[0..4].try_into().unwrap()), stream_id);
        let chunk = &p[4..];
        assert!(chunk.len() <= MAX_SPK_AUDIO_CHUNK);
        assert_eq!(chunk.len() % 2, 0, "chunks must preserve 16-bit sample boundaries");
        received.extend_from_slice(chunk);
    }
    assert_eq!(received, pcm1);

    // Send 'C' barge-in clear (len=0) -> client receives SPK_CLEAR.
    send_jetski_frame(&mut agy, b'C', &[]);
    let (t, p) = client.recv_frame();
    assert_eq!(t, ty::SPK_CLEAR);
    assert_eq!(p, stream_id.to_le_bytes());

    // Send second 'A' chunk after clear -> client receives SPK_AUDIO.
    let pcm2 = vec![0xAAu8; 320];
    send_jetski_frame(&mut agy, b'A', &pcm2);
    let (t, p) = client.recv_frame();
    assert_eq!(t, ty::SPK_AUDIO);
    assert_eq!(u32::from_le_bytes(p[0..4].try_into().unwrap()), stream_id);
    assert_eq!(&p[4..], pcm2.as_slice());

    // Close speaker TCP -> client receives SPK_STOP.
    drop(agy);
    let (t, p) = client.recv_frame();
    assert_eq!(t, ty::SPK_STOP);
    assert_eq!(p, stream_id.to_le_bytes());

    client.close();
    h.stop();
}

#[test]
fn speaker_handshake_err_sanitized_and_closed() {
    let h = VoiceHarness::start(false);
    let mut client = h.attach(None, None).unwrap();

    let mut agy = h.connect_agy();
    agy.write_all(b"JETSKI-AUDIO/1 speaker rate=48000\n").unwrap();
    agy.flush().unwrap();

    let (t, p) = client.recv_frame();
    assert_eq!(t, ty::SPK_START);
    let stream_id = u32::from_le_bytes(p[0..4].try_into().unwrap());
    assert_eq!(u32::from_le_bytes(p[4..8].try_into().unwrap()), 48_000);

    let err_msg = format!("audio\r\nerror {}", "x".repeat(300));
    client.send(ClientMsg::SpkAck { stream_id, ok: false, error: err_msg });

    let mut resp = Vec::new();
    agy.read_to_end(&mut resp).unwrap();
    let text = String::from_utf8(resp).unwrap();
    assert!(text.starts_with("\nJETSKI-AUDIO/1 ERR "), "got {text:?}");
    assert!(text.ends_with('\n'));
    let reason = text.strip_prefix("\nJETSKI-AUDIO/1 ERR ").unwrap().strip_suffix('\n').unwrap();
    assert!(!reason.contains('\n') && !reason.contains('\r'));
    assert!(reason.len() <= 235, "reason len={}", reason.len());
    assert!(reason.starts_with("audio  error xxx"));

    client.close();
    h.stop();
}

#[test]
fn speaker_rejects_bad_clear_len_and_oversized_frame() {
    let h = VoiceHarness::start(false);
    let mut client = h.attach(None, None).unwrap();

    // Case 1: 'C' with len != 0 -> close + SPK_STOP
    let mut agy1 = h.connect_agy();
    agy1.write_all(b"JETSKI-AUDIO/1 speaker rate=24000\n").unwrap();
    let (t, p) = client.recv_frame();
    assert_eq!(t, ty::SPK_START);
    let id1 = u32::from_le_bytes(p[0..4].try_into().unwrap());
    client.send(ClientMsg::SpkAck { stream_id: id1, ok: true, error: String::new() });
    let mut ok_buf = [0u8; 19];
    agy1.read_exact(&mut ok_buf).unwrap();
    assert_eq!(&ok_buf, b"\nJETSKI-AUDIO/1 OK\n");
    send_jetski_frame(&mut agy1, b'C', &[1, 2, 3, 4]);
    let (t, p) = client.recv_frame();
    assert_eq!(t, ty::SPK_STOP);
    assert_eq!(p, id1.to_le_bytes());

    // Case 2: 'A' with len > 1 MiB -> close + SPK_STOP
    let mut agy2 = h.connect_agy();
    agy2.write_all(b"JETSKI-AUDIO/1 speaker rate=24000\n").unwrap();
    let (t, p) = client.recv_frame();
    assert_eq!(t, ty::SPK_START);
    let id2 = u32::from_le_bytes(p[0..4].try_into().unwrap());
    client.send(ClientMsg::SpkAck { stream_id: id2, ok: true, error: String::new() });
    agy2.read_exact(&mut ok_buf).unwrap();
    let mut oversized_hdr = [0u8; 5];
    oversized_hdr[0] = b'A';
    oversized_hdr[1..5].copy_from_slice(&((SPK_FRAME_MAX_LEN as u32) + 1).to_le_bytes());
    agy2.write_all(&oversized_hdr).unwrap();
    let (t, p) = client.recv_frame();
    assert_eq!(t, ty::SPK_STOP);
    assert_eq!(p, id2.to_le_bytes());

    // Case 3: 'A' with odd byte length (malformed s16le) -> close + SPK_STOP
    let mut agy3 = h.connect_agy();
    agy3.write_all(b"JETSKI-AUDIO/1 speaker rate=24000\n").unwrap();
    let (t, p) = client.recv_frame();
    assert_eq!(t, ty::SPK_START);
    let id3 = u32::from_le_bytes(p[0..4].try_into().unwrap());
    client.send(ClientMsg::SpkAck { stream_id: id3, ok: true, error: String::new() });
    agy3.read_exact(&mut ok_buf).unwrap();
    send_jetski_frame(&mut agy3, b'A', &[1, 2, 3]);
    let (t, p) = client.recv_frame();
    assert_eq!(t, ty::SPK_STOP);
    assert_eq!(p, id3.to_le_bytes());

    client.close();
    h.stop();
}

#[test]
fn peek_classification_avoids_false_mic_start() {
    let h = VoiceHarness::start(false);
    let mut client = h.attach(None, None).unwrap();

    // 1. Immediate EOF -> no MIC_START.
    let agy_eof = h.connect_agy();
    drop(agy_eof);

    // 2. Non-JETSKI bytes -> ERR + close, no MIC_START.
    let mut agy_bad = h.connect_agy();
    agy_bad.write_all(b"GET / HTTP/1.1\r\n").unwrap();
    let mut err_buf = Vec::new();
    agy_bad.read_to_end(&mut err_buf).unwrap();
    assert!(err_buf.starts_with(b"\nJETSKI-AUDIO/1 ERR "));

    // 3. Fragmented speaker header -> still classified as speaker, NO MIC_START.
    let mut agy_frag = h.connect_agy();
    agy_frag.write_all(b"JET").unwrap();
    agy_frag.flush().unwrap();
    std::thread::sleep(Duration::from_millis(25));
    agy_frag.write_all(b"SKI-AUDIO/1 speaker rate=24000\n").unwrap();
    agy_frag.flush().unwrap();

    let (t, p) = client.recv_frame();
    assert_eq!(t, ty::SPK_START, "expected SPK_START, never false MIC_START");
    let stream_id = u32::from_le_bytes(p[0..4].try_into().unwrap());
    client.send(ClientMsg::SpkAck { stream_id, ok: true, error: String::new() });
    let mut ok = [0u8; 19];
    agy_frag.read_exact(&mut ok).unwrap();
    assert_eq!(&ok, b"\nJETSKI-AUDIO/1 OK\n");
    drop(agy_frag);
    assert_eq!(client.recv_frame().0, ty::SPK_STOP);

    // Ensure no stray MIC_START arrived after 150ms.
    assert!(client.try_recv_frame(Duration::from_millis(150)).is_none());

    client.close();
    h.stop();
}

#[test]
fn mic_streaming_and_fin_half_close_stop() {
    let h = VoiceHarness::start(false);
    let mut client = h.attach(None, None).unwrap();

    // `agy` opens mic TCP connection and sends nothing.
    let mut agy_mic = h.connect_agy();
    let (t, p) = client.recv_frame();
    assert_eq!(t, ty::MIC_START);
    assert_eq!(p.len(), 9);
    let stream_id = u32::from_le_bytes(p[0..4].try_into().unwrap());
    assert_eq!(u32::from_le_bytes(p[4..8].try_into().unwrap()), 16_000);
    assert_eq!(p[8], 1);

    // Send 20ms (640B) + odd invalid chunk (dropped) + 40ms (1280B).
    let chunk1 = vec![0x12u8; 640];
    let chunk2 = vec![0x34u8; 1280];
    client.send(ClientMsg::MicAudio { stream_id, pcm: chunk1.clone() });
    client.send(ClientMsg::MicAudio { stream_id, pcm: vec![0xFF, 0xFE, 0xFD] }); // odd -> dropped
    client.send(ClientMsg::MicAudio { stream_id, pcm: chunk2.clone() });

    let mut expected = Vec::with_capacity(1920);
    expected.extend_from_slice(&chunk1);
    expected.extend_from_slice(&chunk2);
    let mut got = vec![0u8; 1920];
    agy_mic.read_exact(&mut got).unwrap();
    assert_eq!(got, expected);

    // `agy` stops recording by half-closing write end (FIN) and draining read until EOF.
    agy_mic.shutdown(Shutdown::Write).unwrap();
    let mut drain = Vec::new();
    agy_mic.read_to_end(&mut drain).unwrap();
    assert!(drain.is_empty(), "expected clean EOF after FIN");

    let (t, p) = client.recv_frame();
    assert_eq!(t, ty::MIC_STOP);
    assert_eq!(p, stream_id.to_le_bytes());

    client.close();
    h.stop();
}

#[test]
fn concurrent_mic_and_speaker_live_mode() {
    let h = VoiceHarness::start(false);
    let mut client = h.attach(None, None).unwrap();

    // Open mic AND speaker concurrently (as in `/voice live`).
    let mut agy_mic = h.connect_agy();
    let mut agy_spk = h.connect_agy();
    agy_spk.write_all(b"JETSKI-AUDIO/1 speaker rate=24000\n").unwrap();
    agy_spk.flush().unwrap();

    // Collect both SPK_START and MIC_START (speaker arrives first because peek resolves immediately).
    let mut mic_id = None;
    let mut spk_id = None;
    while mic_id.is_none() || spk_id.is_none() {
        let (t, p) = client.recv_frame();
        match t {
            ty::SPK_START => {
                let id = u32::from_le_bytes(p[0..4].try_into().unwrap());
                spk_id = Some(id);
                client.send(ClientMsg::SpkAck { stream_id: id, ok: true, error: String::new() });
            }
            ty::MIC_START => {
                mic_id = Some(u32::from_le_bytes(p[0..4].try_into().unwrap()));
            }
            other => panic!("unexpected frame 0x{other:02x}"),
        }
    }
    let (mic_id, spk_id) = (mic_id.unwrap(), spk_id.unwrap());
    assert_ne!(mic_id, spk_id);

    let mut ok = [0u8; 19];
    agy_spk.read_exact(&mut ok).unwrap();
    assert_eq!(&ok, b"\nJETSKI-AUDIO/1 OK\n");

    // Stream simultaneously in both directions.
    let up_pcm = vec![0x55u8; 640];
    let down_pcm = vec![0x77u8; 480];
    client.send(ClientMsg::MicAudio { stream_id: mic_id, pcm: up_pcm.clone() });
    send_jetski_frame(&mut agy_spk, b'A', &down_pcm);

    let mut agy_got = vec![0u8; 640];
    agy_mic.read_exact(&mut agy_got).unwrap();
    assert_eq!(agy_got, up_pcm);

    let (t, p) = client.recv_frame();
    assert_eq!(t, ty::SPK_AUDIO);
    assert_eq!(u32::from_le_bytes(p[0..4].try_into().unwrap()), spk_id);
    assert_eq!(&p[4..], down_pcm.as_slice());

    // Close both streams cleanly.
    agy_mic.shutdown(Shutdown::Write).unwrap();
    drop(agy_spk);
    let mut got_mic_stop = false;
    let mut got_spk_stop = false;
    while !(got_mic_stop && got_spk_stop) {
        let (t, p) = client.recv_frame();
        let id = u32::from_le_bytes(p[0..4].try_into().unwrap());
        match t {
            ty::MIC_STOP if id == mic_id => got_mic_stop = true,
            ty::SPK_STOP if id == spk_id => got_spk_stop = true,
            other => panic!("unexpected frame 0x{other:02x}"),
        }
    }

    client.close();
    h.stop();
}

#[test]
fn resume_takeover_preserves_streams_and_ignores_duplicate_spk_ack() {
    let h = VoiceHarness::start(false);
    let mut c1 = h.attach(None, None).unwrap();
    let sess = c1.hello.session.clone();
    let tok = c1.hello.resume_token.clone();
    let port1 = c1.hello.mic_port;

    // Unauthorized resume with wrong token must be rejected and NOT kick c1.
    assert!(h.attach(Some(&sess), Some("wrong-token")).is_err());
    c1.send(ClientMsg::Ping(99));
    let (t, p) = c1.recv_frame();
    assert_eq!(t, ty::PONG);
    assert_eq!(u64::from_be_bytes(p[0..8].try_into().unwrap()), 99);

    // Open active speaker + mic streams on c1.
    let mut agy_spk = h.connect_agy();
    agy_spk.write_all(b"JETSKI-AUDIO/1 speaker rate=24000\n").unwrap();
    let (t, p) = c1.recv_frame();
    assert_eq!(t, ty::SPK_START);
    let spk_id = u32::from_le_bytes(p[0..4].try_into().unwrap());
    c1.send(ClientMsg::SpkAck { stream_id: spk_id, ok: true, error: String::new() });
    let mut ok = [0u8; 19];
    agy_spk.read_exact(&mut ok).unwrap();
    assert_eq!(&ok, b"\nJETSKI-AUDIO/1 OK\n");

    let mut agy_mic = h.connect_agy();
    let (t, p) = c1.recv_frame();
    assert_eq!(t, ty::MIC_START);
    let mic_id = u32::from_le_bytes(p[0..4].try_into().unwrap());

    // Client 2 takes over with valid --resume and --token while c1 is still attached.
    let mut c2 = h.attach(Some(&sess), Some(&tok)).unwrap();
    assert_eq!(c2.hello.mic_port, port1);
    assert_eq!(c2.hello.session, sess);
    assert_eq!(c2.hello.resume_token, tok);

    // c2 immediately receives re-emitted MIC_START and SPK_START for the active streams.
    let mut saw_mic = false;
    let mut saw_spk = false;
    for _ in 0..2 {
        let (t, p) = c2.recv_frame();
        match t {
            ty::MIC_START => {
                assert_eq!(u32::from_le_bytes(p[0..4].try_into().unwrap()), mic_id);
                saw_mic = true;
            }
            ty::SPK_START => {
                assert_eq!(u32::from_le_bytes(p[0..4].try_into().unwrap()), spk_id);
                saw_spk = true;
            }
            other => panic!("unexpected re-emitted frame 0x{other:02x}"),
        }
    }
    assert!(saw_mic && saw_spk);

    // c2 sends duplicate SPK_ACK(ok=1): plugin MUST NOT send a second "\nJETSKI-AUDIO/1 OK\n" to agy!
    c2.send(ClientMsg::SpkAck { stream_id: spk_id, ok: true, error: String::new() });
    agy_spk.set_read_timeout(Some(Duration::from_millis(120))).unwrap();
    let mut extra = [0u8; 32];
    match agy_spk.read(&mut extra) {
        Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
        other => panic!("unexpected extra bytes written to agy_spk after duplicate SPK_ACK: {other:?}"),
    }

    // Both streams continue working seamlessly through c2!
    let down = vec![0x42u8; 400];
    send_jetski_frame(&mut agy_spk, b'A', &down);
    let (t, p) = c2.recv_frame();
    assert_eq!(t, ty::SPK_AUDIO);
    assert_eq!(u32::from_le_bytes(p[0..4].try_into().unwrap()), spk_id);
    assert_eq!(&p[4..], down.as_slice());

    let up = vec![0x24u8; 640];
    c2.send(ClientMsg::MicAudio { stream_id: mic_id, pcm: up.clone() });
    let mut agy_mic_got = vec![0u8; 640];
    agy_mic.read_exact(&mut agy_mic_got).unwrap();
    assert_eq!(agy_mic_got, up);

    drop(agy_spk);
    drop(agy_mic);
    c1.close();
    c2.close();
    h.stop();
}

#[test]
fn udp_transport_roundtrip_and_ssh_fallback() {
    let h = VoiceHarness::start(true);
    let mut client = h.attach(None, None).unwrap();
    let udp_info = client.hello.udp.clone().expect("UDP link should bind");

    // Decode 16-byte base64 key.
    let key = decode_b64_16(&udp_info.key);
    let crypto = Crypto::new(&key);
    let sock = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let peer: SocketAddr = ([127, 0, 0, 1], udp_info.port).into();

    let mut c2s_seq = 1u64;
    let send_udp = |seq: &mut u64, pkt: &Packet| {
        let datagram = crypto.seal(DIR_C2S, *seq, &pkt.encode());
        *seq += 1;
        sock.send_to(&datagram, peer).unwrap();
    };

    // Bring UDP link alive with a heartbeat.
    send_udp(&mut c2s_seq, &Packet::Heartbeat { t: 1000, echo: 0 });
    let mut reasm = Reassembler::default();
    let mut recv_udp_frame = |seq: &mut u64| -> (u8, Vec<u8>) {
        let mut buf = [0u8; 2048];
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let (n, _) = sock.recv_from(&mut buf).unwrap();
            let Some((dir, _s, plain)) = crypto.open(&buf[..n]) else { continue };
            assert_eq!(dir, DIR_S2C);
            let Some(pkt) = Packet::decode(&plain) else { continue };
            if let Packet::Frag(f) = pkt {
                let reliable = f.flags & FLAG_RELIABLE != 0;
                let msg = f.msg;
                if let Some(d) = reasm.push(f, crate::serve::now_us()) {
                    if reliable {
                        send_udp(seq, &Packet::Rack(vec![msg]));
                    }
                    return proto::read_frame(&mut d.bytes.as_slice()).unwrap().unwrap();
                }
            }
        }
        panic!("timed out waiting for UDP voice frame");
    };
    // Wait for heartbeat echo so we know host marked UDP peer alive.
    {
        let mut buf = [0u8; 2048];
        let (n, _) = sock.recv_from(&mut buf).unwrap();
        let (dir, _, plain) = crypto.open(&buf[..n]).unwrap();
        assert_eq!(dir, DIR_S2C);
        assert!(matches!(Packet::decode(&plain), Some(Packet::Heartbeat { echo: 1000, .. })));
    }

    // Speaker stream over UDP!
    let mut agy_spk = h.connect_agy();
    agy_spk.write_all(b"JETSKI-AUDIO/1 speaker rate=24000\n").unwrap();
    let (t, p) = recv_udp_frame(&mut c2s_seq);
    assert_eq!(t, ty::SPK_START);
    let spk_id = u32::from_le_bytes(p[0..4].try_into().unwrap());

    // Send SPK_ACK reliably (msg=1) over UDP.
    let mut ack_frame = Vec::new();
    let (at, ap) = proto::encode_client(&ClientMsg::SpkAck { stream_id: spk_id, ok: true, error: String::new() });
    proto::write_frame(&mut ack_frame, at, &ap).unwrap();
    for f in udp::fragment(1, FLAG_RELIABLE, &ack_frame, 1.0) {
        send_udp(&mut c2s_seq, &Packet::Frag(f));
    }

    let mut ok = [0u8; 19];
    agy_spk.read_exact(&mut ok).unwrap();
    assert_eq!(&ok, b"\nJETSKI-AUDIO/1 OK\n");

    let pcm = vec![0x33u8; 480];
    send_jetski_frame(&mut agy_spk, b'A', &pcm);
    let (t, p) = recv_udp_frame(&mut c2s_seq);
    assert_eq!(t, ty::SPK_AUDIO);
    assert_eq!(u32::from_le_bytes(p[0..4].try_into().unwrap()), spk_id);
    assert_eq!(&p[4..], pcm.as_slice());

    // Disable UDP via CONFIG -> SPK_CLEAR must fall back seamlessly to SSH stream!
    client.send(ClientMsg::Config(proto::Config { udp: Some(false), ..Default::default() }));
    client.send(ClientMsg::Ping(7));
    assert_eq!(client.recv_frame().0, ty::PONG);

    send_jetski_frame(&mut agy_spk, b'C', &[]);
    let (t, p) = client.recv_frame();
    assert_eq!(t, ty::SPK_CLEAR);
    assert_eq!(p, spk_id.to_le_bytes());

    drop(agy_spk);
    client.close();
    h.stop();
}

#[test]
fn speaker_open_without_attached_client_reports_no_client_connected() {
    let h = VoiceHarness::start(false);
    // Connect agy speaker while NO mobile client is attached.
    let mut agy = h.connect_agy();
    agy.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    agy.write_all(b"JETSKI-AUDIO/1 speaker rate=24000\n").unwrap();
    agy.flush().unwrap();

    let mut resp = Vec::new();
    agy.read_to_end(&mut resp).unwrap();
    let text = String::from_utf8(resp).unwrap();
    assert_eq!(text, "\nJETSKI-AUDIO/1 ERR no voice client connected\n");
    h.stop();
}

struct StalledWriter {
    gate: Arc<(Mutex<bool>, Condvar)>,
    entered: Arc<AtomicBool>,
    allow_initial_bytes: usize,
    written: usize,
}

impl Write for StalledWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written < self.allow_initial_bytes {
            let take = buf.len().min(self.allow_initial_bytes - self.written);
            self.written += take;
            return Ok(take);
        }
        self.entered.store(true, Ordering::SeqCst);
        let (lock, cvar) = &*self.gate;
        let mut released = lock.lock().unwrap();
        while !*released {
            released = cvar.wait(released).unwrap();
        }
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "stalled writer released"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn stalled_ssh_writer_does_not_block_engine_or_resume_takeover() {
    let h = VoiceHarness::start(false);
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let entered = Arc::new(AtomicBool::new(false));

    // Attach client 1 whose output stalls permanently right after initial writes.
    let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let mut c1_in = TcpStream::connect(l.local_addr().unwrap()).unwrap();
    let (server_in, _) = l.accept().unwrap();
    let server_close = server_in.try_clone().unwrap();
    let (reply_tx, reply_rx) = mpsc::channel::<bool>();
    h.ev_tx
        .send(Event::ClientAttach {
            req: AttachReq {
                ipc_token: String::new(),
                proto: proto::PROTO,
                resume: None,
                token: None,
                idle_timeout: None,
                ssh_client: None,
                evict_idle_port: None,
                shutdown: false,
            },
            input: Box::new(server_in),
            output: Box::new(StalledWriter {
                gate: gate.clone(),
                entered: entered.clone(),
                allow_initial_bytes: 0,
                written: 0,
            }),
            closer: Box::new(move || { let _ = server_close.shutdown(Shutdown::Both); }),
            reply_tx,
        })
        .unwrap();
    assert!(reply_rx.recv_timeout(Duration::from_secs(2)).unwrap());

    // Wait for SshWriter thread to enter the blocked write() call.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !entered.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(entered.load(Ordering::SeqCst), "stalled writer should be blocked inside write()");

    // Open speaker stream, ACK it from c1, and flood > 300 SPK_AUDIO chunks (> SSH_QUEUE_CAPACITY)
    // while c1's SSH writer is completely wedged.
    let mut agy = h.connect_agy();
    agy.write_all(b"JETSKI-AUDIO/1 speaker rate=24000\n").unwrap();
    std::thread::sleep(Duration::from_millis(40));
    let (at, ap) = proto::encode_client(&ClientMsg::SpkAck { stream_id: 1, ok: true, error: String::new() });
    proto::write_frame(&mut c1_in, at, &ap).unwrap();
    c1_in.flush().unwrap();
    let mut ok = [0u8; 19];
    agy.read_exact(&mut ok).unwrap();
    assert_eq!(&ok, b"\nJETSKI-AUDIO/1 OK\n");

    let big_pcm = vec![0x5Au8; MAX_SPK_AUDIO_CHUNK * 300];
    send_jetski_frame(&mut agy, b'A', &big_pcm);

    // Despite c1's writer being permanently wedged AND flooded, --resume takeover MUST succeed
    // immediately without blocking VoiceEngine!
    let t0 = Instant::now();
    let mut c2 = h
        .attach(Some("testvoice1234567"), Some("test-resume-token-secret"))
        .expect("resume takeover should succeed promptly");
    assert!(
        t0.elapsed() < Duration::from_secs(1),
        "resume takeover took too long: {:?}",
        t0.elapsed()
    );

    // c2 receives re-announced SPK_START and subsequent SPK_AUDIO cleanly.
    let (t, p) = c2.recv_frame();
    assert_eq!(t, ty::SPK_START);
    let spk_id = u32::from_le_bytes(p[0..4].try_into().unwrap());
    assert_eq!(spk_id, 1);

    let tail = vec![0x7Eu8; 320];
    send_jetski_frame(&mut agy, b'A', &tail);
    let mut saw_tail = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let (t, p) = c2.recv_frame();
        if t == ty::SPK_AUDIO && &p[4..] == tail.as_slice() {
            saw_tail = true;
            break;
        }
    }
    assert!(saw_tail, "resumed client should receive fresh SPK_AUDIO");

    // Release stalled writer so background thread cleans up.
    {
        let (lock, cvar) = &*gate;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }
    drop(agy);
    drop(c1_in);
    c2.close();
    h.stop();
}

#[test]
fn udp_only_keepalive_after_ssh_drops_resists_eviction_and_reannounces() {
    let h = VoiceHarness::start(true);
    let client = h.attach(None, None).unwrap();
    let udp_info = client.hello.udp.clone().expect("UDP link should bind");
    let link = h.last_udp.lock().unwrap().clone().expect("captured UdpLink");

    let key = decode_b64_16(&udp_info.key);
    let crypto = Crypto::new(&key);
    let sock = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let peer: SocketAddr = ([127, 0, 0, 1], udp_info.port).into();

    let mut c2s_seq = 1u64;
    let send_udp = |seq: &mut u64, pkt: &Packet| {
        let datagram = crypto.seal(DIR_C2S, *seq, &pkt.encode());
        *seq += 1;
        sock.send_to(&datagram, peer).unwrap();
    };

    // Bring UDP alive and wait for heartbeat echo.
    send_udp(&mut c2s_seq, &Packet::Heartbeat { t: 100, echo: 0 });
    let mut buf = [0u8; 2048];
    let _ = sock.recv_from(&mut buf).unwrap();

    // Drop SSH while UDP is active -> engine must stay connected on UDP.
    client.close();
    std::thread::sleep(Duration::from_millis(60));

    // Simulate 5s of UDP silence (inside 3s..30s UDP_KEEPALIVE_US roaming window).
    link.advance_clock_us(5_000_000);
    assert!(!link.alive());

    // EvictIdle MUST NOT evict during the 30s UDP keepalive window!
    let (reply_tx, reply_rx) = mpsc::channel();
    let (_ftx, flushed_rx) = mpsc::channel();
    h.ev_tx
        .send(Event::EvictIdle { port: h.mic_port, reply_tx, flushed_rx })
        .unwrap();
    assert!(!reply_rx.recv_timeout(Duration::from_secs(1)).unwrap());

    // Even while !l.alive() in the 3s..30s window, ClientOut::emit still attempts UDP delivery.
    let agy_mic1 = h.connect_agy();
    let mut reasm = Reassembler::default();
    let mut saw_mic1 = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let (n, _) = sock.recv_from(&mut buf).unwrap();
        let Some((DIR_S2C, _, plain)) = crypto.open(&buf[..n]) else { continue };
        if let Some(Packet::Frag(f)) = Packet::decode(&plain) {
            let msg = f.msg;
            if let Some(d) = reasm.push(f, crate::serve::now_us()) {
                let (t, _) = proto::read_frame(&mut d.bytes.as_slice()).unwrap().unwrap();
                if t == ty::MIC_START {
                    send_udp(&mut c2s_seq, &Packet::Rack(vec![msg]));
                    saw_mic1 = true;
                    break;
                }
            }
        }
    }
    assert!(saw_mic1, "MIC_START should still be attempted over UDP during 3s..30s keepalive");

    // Now simulate total packet loss during roaming so a new mic stream's 5 UDP retries expire,
    // clearing announced_to_epoch; once UDP returns to alive(), it MUST be re-announced!
    link.advance_clock_us(5_000_000);
    link.drop_ppm.store(1_000_000, Ordering::Relaxed);
    let agy_mic2 = h.connect_agy();
    // Wait for peek (80ms) + 5 retries * 80ms (~480ms) + margin so reliable retries exhaust.
    std::thread::sleep(Duration::from_millis(850));

    // Client roams back online: restore delivery and send a heartbeat so UDP becomes alive().
    link.drop_ppm.store(0, Ordering::Relaxed);
    send_udp(&mut c2s_seq, &Packet::Heartbeat { t: 200, echo: 0 });
    let mut saw_mic2 = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let (n, _) = sock.recv_from(&mut buf).unwrap();
        let Some((DIR_S2C, _, plain)) = crypto.open(&buf[..n]) else { continue };
        if let Some(Packet::Frag(f)) = Packet::decode(&plain) {
            let msg = f.msg;
            if let Some(d) = reasm.push(f, crate::serve::now_us()) {
                let (t, p) = proto::read_frame(&mut d.bytes.as_slice()).unwrap().unwrap();
                if t == ty::MIC_START && u32::from_le_bytes(p[0..4].try_into().unwrap()) == 2 {
                    send_udp(&mut c2s_seq, &Packet::Rack(vec![msg]));
                    saw_mic2 = true;
                    break;
                }
            }
        }
    }
    assert!(saw_mic2, "unannounced mic stream must re-announce once UDP returns to alive()");

    // Wait briefly so the RACK sent above is processed before we advance the link clock.
    std::thread::sleep(Duration::from_millis(50));
    // After >30s UDP silence, engine disconnects client and EvictIdle succeeds.
    link.advance_clock_us(31_000_000);
    std::thread::sleep(Duration::from_millis(120));
    let (reply_tx, reply_rx) = mpsc::channel();
    let (_ftx, flushed_rx) = mpsc::channel();
    h.ev_tx
        .send(Event::EvictIdle { port: h.mic_port, reply_tx, flushed_rx })
        .unwrap();
    assert!(reply_rx.recv_timeout(Duration::from_secs(1)).unwrap());

    drop(agy_mic1);
    drop(agy_mic2);
    h.stop();
}

#[test]
fn reliable_control_frame_udp_tick_falls_back_to_ssh() {
    let h = VoiceHarness::start(true);
    let mut client = h.attach(None, None).unwrap();
    let udp_info = client.hello.udp.clone().expect("UDP link should bind");
    let link = h.last_udp.lock().unwrap().clone().expect("captured UdpLink");

    let key = decode_b64_16(&udp_info.key);
    let crypto = Crypto::new(&key);
    let sock = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let peer: SocketAddr = ([127, 0, 0, 1], udp_info.port).into();

    // Bring UDP alive so SPK_START is initially sent on UDP.
    let hb = crypto.seal(DIR_C2S, 1, &Packet::Heartbeat { t: 100, echo: 0 }.encode());
    sock.send_to(&hb, peer).unwrap();
    let mut buf = [0u8; 2048];
    let _ = sock.recv_from(&mut buf).unwrap();

    // Blackhole S->C UDP packets while UDP is still considered alive (< 3s).
    link.drop_ppm.store(1_000_000, Ordering::Relaxed);

    let mut agy = h.connect_agy();
    agy.write_all(b"JETSKI-AUDIO/1 speaker rate=24000\n").unwrap();
    agy.flush().unwrap();

    // After 5 UDP retries (~450ms), VoiceEngine must automatically fall back SPK_START to SSH!
    let (t, p) = client.recv_frame();
    assert_eq!(t, ty::SPK_START);
    let spk_id = u32::from_le_bytes(p[0..4].try_into().unwrap());
    client.send(ClientMsg::SpkAck { stream_id: spk_id, ok: true, error: String::new() });

    let mut ok = [0u8; 19];
    agy.read_exact(&mut ok).unwrap();
    assert_eq!(&ok, b"\nJETSKI-AUDIO/1 OK\n");

    drop(agy);
    client.close();
    h.stop();
}

#[test]
fn resumed_client_spk_ack_false_closes_active_speaker_stream() {
    let h = VoiceHarness::start(false);
    let mut c1 = h.attach(None, None).unwrap();
    let sess = c1.hello.session.clone();
    let tok = c1.hello.resume_token.clone();

    let mut agy = h.connect_agy();
    agy.write_all(b"JETSKI-AUDIO/1 speaker rate=24000\n").unwrap();
    let (t, p) = c1.recv_frame();
    assert_eq!(t, ty::SPK_START);
    let spk_id = u32::from_le_bytes(p[0..4].try_into().unwrap());
    c1.send(ClientMsg::SpkAck { stream_id: spk_id, ok: true, error: String::new() });
    let mut ok = [0u8; 19];
    agy.read_exact(&mut ok).unwrap();
    assert_eq!(&ok, b"\nJETSKI-AUDIO/1 OK\n");

    // Client 2 resumes and rejects the already-open speaker stream with SPK_ACK(ok=false).
    let mut c2 = h.attach(Some(&sess), Some(&tok)).unwrap();
    let (t, p) = c2.recv_frame();
    assert_eq!(t, ty::SPK_START);
    assert_eq!(u32::from_le_bytes(p[0..4].try_into().unwrap()), spk_id);
    c2.send(ClientMsg::SpkAck { stream_id: spk_id, ok: false, error: "output busy".into() });

    // `agy` speaker TCP stream must close cleanly (EOF, no bogus second header line).
    let mut rest = Vec::new();
    agy.read_to_end(&mut rest).unwrap();
    assert!(rest.is_empty(), "expected clean EOF without extra ERR line, got {rest:?}");

    c1.close();
    c2.close();
    h.stop();
}

#[test]
fn speaker_framing_edge_cases_channels_timeout_empty_and_truncated() {
    let h = VoiceHarness::start(false);
    let mut client = h.attach(None, None).unwrap();

    // 1. channels=2 rejected before SPK_START.
    let mut agy_ch2 = h.connect_agy();
    agy_ch2.write_all(b"JETSKI-AUDIO/1 speaker rate=24000 channels=2\n").unwrap();
    let mut err_buf = Vec::new();
    agy_ch2.read_to_end(&mut err_buf).unwrap();
    assert_eq!(
        String::from_utf8(err_buf).unwrap(),
        "\nJETSKI-AUDIO/1 ERR unsupported channel count\n"
    );

    // 2. Empty 'A' frame (len=0) is skipped cleanly without closing stream, followed by valid audio.
    let mut agy_empty = h.connect_agy();
    agy_empty.write_all(b"JETSKI-AUDIO/1 speaker rate=24000 channels=1\n").unwrap();
    let (t, p) = client.recv_frame();
    assert_eq!(t, ty::SPK_START);
    let id1 = u32::from_le_bytes(p[0..4].try_into().unwrap());
    client.send(ClientMsg::SpkAck { stream_id: id1, ok: true, error: String::new() });
    let mut ok = [0u8; 19];
    agy_empty.read_exact(&mut ok).unwrap();
    assert_eq!(&ok, b"\nJETSKI-AUDIO/1 OK\n");
    send_jetski_frame(&mut agy_empty, b'A', &[]);
    let pcm = vec![0x99u8; 320];
    send_jetski_frame(&mut agy_empty, b'A', &pcm);
    let (t, p) = client.recv_frame();
    assert_eq!(t, ty::SPK_AUDIO);
    assert_eq!(&p[4..], pcm.as_slice());
    drop(agy_empty);
    assert_eq!(client.recv_frame().0, ty::SPK_STOP);

    // 3. Truncated 'A' payload hitting byte-0 EOF on second streamed chunk (len=1920, only 960 sent).
    let mut agy_trunc1 = h.connect_agy();
    agy_trunc1.write_all(b"JETSKI-AUDIO/1 speaker rate=24000\n").unwrap();
    let (t, p) = client.recv_frame();
    assert_eq!(t, ty::SPK_START);
    let id2 = u32::from_le_bytes(p[0..4].try_into().unwrap());
    client.send(ClientMsg::SpkAck { stream_id: id2, ok: true, error: String::new() });
    agy_trunc1.read_exact(&mut ok).unwrap();
    let mut hdr = [0u8; 5];
    hdr[0] = b'A';
    hdr[1..5].copy_from_slice(&(1920u32).to_le_bytes());
    agy_trunc1.write_all(&hdr).unwrap();
    agy_trunc1.write_all(&vec![0x11u8; MAX_SPK_AUDIO_CHUNK]).unwrap();
    agy_trunc1.flush().unwrap();
    drop(agy_trunc1);
    let (t, p) = client.recv_frame();
    assert_eq!(t, ty::SPK_AUDIO);
    assert_eq!(p.len(), 4 + MAX_SPK_AUDIO_CHUNK);
    let (t, p) = client.recv_frame();
    assert_eq!(t, ty::SPK_STOP);
    assert_eq!(p, id2.to_le_bytes());

    // 4. Truncated 'A' payload mid-chunk (len=400, only 100 sent).
    let mut agy_trunc2 = h.connect_agy();
    agy_trunc2.write_all(b"JETSKI-AUDIO/1 speaker rate=24000\n").unwrap();
    let (t, p) = client.recv_frame();
    assert_eq!(t, ty::SPK_START);
    let id3 = u32::from_le_bytes(p[0..4].try_into().unwrap());
    client.send(ClientMsg::SpkAck { stream_id: id3, ok: true, error: String::new() });
    agy_trunc2.read_exact(&mut ok).unwrap();
    hdr[1..5].copy_from_slice(&(400u32).to_le_bytes());
    agy_trunc2.write_all(&hdr).unwrap();
    agy_trunc2.write_all(&[0x22u8; 100]).unwrap();
    agy_trunc2.flush().unwrap();
    drop(agy_trunc2);
    let (t, p) = client.recv_frame();
    assert_eq!(t, ty::SPK_STOP);
    assert_eq!(p, id3.to_le_bytes());

    // 5. Header timeout when peer sends partial speaker header without newline.
    let mut agy_to = h.connect_agy();
    agy_to.set_read_timeout(Some(Duration::from_secs(6))).unwrap();
    agy_to.write_all(b"JETSKI-AUDIO/1 speaker rate=24000").unwrap();
    agy_to.flush().unwrap();
    let mut to_buf = Vec::new();
    agy_to.read_to_end(&mut to_buf).unwrap();
    assert_eq!(
        String::from_utf8(to_buf).unwrap(),
        "\nJETSKI-AUDIO/1 ERR header timeout\n"
    );

    client.close();
    h.stop();
}

#[test]
fn handle_attach_failure_preserves_disconnected_since_and_exits_idle() {
    let mut h = VoiceHarness::start_with_idle(false, Duration::from_millis(150));
    let c1 = h.attach(None, None).unwrap();
    let sess = c1.hello.session.clone();
    let tok = c1.hello.resume_token.clone();

    // Send a valid resume attach whose `reply_rx` is immediately dropped so `reply_tx.send(true)`
    // fails AFTER `self.client.take()` closed `c1`. `disconnected_since` MUST be set so the
    // engine exits after `idle_timeout` instead of leaking forever.
    let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let _peer = TcpStream::connect(l.local_addr().unwrap()).unwrap();
    let (server, _) = l.accept().unwrap();
    let server_in = server.try_clone().unwrap();
    let server_close = server.try_clone().unwrap();
    let (reply_tx, reply_rx) = mpsc::channel::<bool>();
    drop(reply_rx);
    h.ev_tx
        .send(Event::ClientAttach {
            req: AttachReq {
                ipc_token: String::new(),
                proto: proto::PROTO,
                resume: Some(sess),
                token: Some(tok),
                idle_timeout: None,
                ssh_client: None,
                evict_idle_port: None,
                shutdown: false,
            },
            input: Box::new(server_in),
            output: Box::new(server),
            closer: Box::new(move || { let _ = server_close.shutdown(Shutdown::Both); }),
            reply_tx,
        })
        .unwrap();

    // Wait for engine worker to exit cleanly on idle timeout (150ms) without calling h.stop()!
    let worker = h.worker.take().unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = worker.join();
        let _ = done_tx.send(());
    });
    assert!(
        done_rx.recv_timeout(Duration::from_secs(2)).is_ok(),
        "VoiceEngine leaked after handle_attach early return"
    );
    c1.close();
}

#[test]
fn newborn_engine_not_evicted_before_first_attach() {
    let h = VoiceHarness::start(false);
    let (reply_tx, reply_rx) = mpsc::channel();
    let (_ftx, flushed_rx) = mpsc::channel();
    h.ev_tx
        .send(Event::EvictIdle { port: h.mic_port, reply_tx, flushed_rx })
        .unwrap();
    assert!(
        !reply_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        "newborn daemon must not be evicted before initial attach"
    );
    // Initial attach still succeeds afterwards.
    let c = h.attach(None, None).unwrap();
    c.close();
    h.stop();
}

#[test]
fn mic_fin_half_close_flushes_queued_tail_pcm() {
    let h = VoiceHarness::start(false);
    let mut client = h.attach(None, None).unwrap();
    let mut agy_mic = h.connect_agy();
    let (t, p) = client.recv_frame();
    assert_eq!(t, ty::MIC_START);
    let mic_id = u32::from_le_bytes(p[0..4].try_into().unwrap());

    // Push several chunks and immediately FIN-close agy's write half before reading them.
    let tail = vec![0x42u8; 640];
    for _ in 0..5 {
        client.send(ClientMsg::MicAudio { stream_id: mic_id, pcm: tail.clone() });
    }
    std::thread::sleep(Duration::from_millis(50));
    agy_mic.shutdown(Shutdown::Write).unwrap();

    let mut got = Vec::new();
    agy_mic.read_to_end(&mut got).unwrap();
    assert_eq!(got.len(), 640 * 5, "all queued tail PCM chunks must flush before FIN");
    assert_eq!(client.recv_frame().0, ty::MIC_STOP);

    client.close();
    h.stop();
}

#[test]
fn force_shutdown_evicts_active_session_for_uninstall() {
    let mut h = VoiceHarness::start(false);
    let c = h.attach(None, None).unwrap();
    let (reply_tx, reply_rx) = mpsc::channel();
    let (_ftx, flushed_rx) = mpsc::channel();
    h.ev_tx
        .send(Event::ForceShutdown { reply_tx, flushed_rx })
        .unwrap();
    assert!(reply_rx.recv_timeout(Duration::from_secs(2)).unwrap());
    let worker = h.worker.take().unwrap();
    worker.join().unwrap();
    c.close();
    h.stop();
}

fn decode_b64_16(s: &str) -> [u8; 16] {
    fn val(b: u8) -> u8 {
        match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => 0,
        }
    }
    let mut out = Vec::new();
    for c in s.as_bytes().chunks(4) {
        let n = ((val(c[0]) as u32) << 18) | ((val(c[1]) as u32) << 12) | ((val(c[2]) as u32) << 6) | (val(c[3]) as u32);
        out.push((n >> 16) as u8);
        if c[2] != b'=' { out.push((n >> 8) as u8); }
        if c[3] != b'=' { out.push(n as u8); }
    }
    out.try_into().unwrap()
}
