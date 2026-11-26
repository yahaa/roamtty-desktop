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
    assert_eq!(parse_speaker_header("JETSKI-AUDIO/1 speaker rate=48000 extra=1\n"), Ok(48000));
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
    ev_tx: mpsc::Sender<Event>,
    shutdown: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
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
        let (mic_listener, mic_port) = bind_mic_listener(0).unwrap();
        let (ev_tx, ev_rx) = mpsc::channel::<Event>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let next_stream_id = Arc::new(AtomicU32::new(1));
        spawn_mic_accept_loop(mic_listener, next_stream_id, ev_tx.clone(), shutdown.clone());

        let sess = Session {
            session: "testvoice1234567".into(),
            resume_token: "test-resume-token-secret".into(),
        };
        let bind_udp: UdpBinder = if enable_udp {
            Box::new(UdpLink::bind_ephemeral)
        } else {
            Box::new(|_| None)
        };
        let mut engine = VoiceEngine::new(
            mic_port,
            sess,
            Duration::from_secs(60),
            bind_udp,
            ev_tx.clone(),
            shutdown.clone(),
        );
        let worker = std::thread::spawn(move || engine.run(ev_rx));
        Self { mic_port, ev_tx, shutdown, worker: Some(worker) }
    }

    fn attach(&self, resume: Option<&str>, token: Option<&str>) -> Result<ClientConn, ()> {
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let client = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (server, _) = l.accept().unwrap();
        client.set_nodelay(true).unwrap();
        server.set_nodelay(true).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();

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

    fn stop(mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.ev_tx.send(Event::Shutdown);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

impl Drop for VoiceHarness {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.ev_tx.send(Event::Shutdown);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
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
