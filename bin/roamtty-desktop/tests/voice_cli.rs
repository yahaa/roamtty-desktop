//! End-to-end subprocess integration tests against the compiled `roamtty-desktop` binary.

use proto::{ty, ClientMsg, StartError, VoiceHello};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
fn cli_voice_subprocess_resume_concurrent_and_audio_roundtrip() {
    let exe = env!("CARGO_BIN_EXE_roamtty-desktop");
    let root = std::env::temp_dir().join(format!("roamtty-voice-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    // 1. Unsupported --proto returns StartError JSON and exits 1.
    let bad = Command::new(exe)
        .args(["voice", "--proto", "99"])
        .env("ROAMTTY_DESKTOP_ROOT", &root)
        .output()
        .unwrap();
    assert_eq!(bad.status.code(), Some(1));
    let err: StartError = serde_json::from_slice(&bad.stdout).unwrap();
    assert_eq!(err.error, "proto_unsupported");
    assert_eq!(err.min, Some(proto::PROTO));

    // 2. Spawn session 1 with ephemeral port and 10s idle keepalive.
    let mut p1 = Command::new(exe)
        .args(["voice", "--proto", "1", "--port", "0", "--idle-timeout", "10"])
        .env("ROAMTTY_DESKTOP_ROOT", &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut p1_out = BufReader::new(p1.stdout.take().unwrap());
    let mut hello1_line = String::new();
    p1_out.read_line(&mut hello1_line).unwrap();
    let hello1: VoiceHello = serde_json::from_str(&hello1_line).unwrap();
    assert_eq!(hello1.proto, 1);
    assert_eq!(hello1.mode, "voice");
    assert_ne!(hello1.mic_port, 0);
    assert_eq!(hello1.codecs, vec!["opus", "pcm16"]);
    assert!(hello1.udp.is_some());

    // 3. Concurrent second fresh `voice` requesting session 1's exact port must coexist on a fallback port!
    let mut p_other = Command::new(exe)
        .args(["voice", "--proto", "1", "--port", &hello1.mic_port.to_string(), "--idle-timeout", "0"])
        .env("ROAMTTY_DESKTOP_ROOT", &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut po_out = BufReader::new(p_other.stdout.take().unwrap());
    let mut po_line = String::new();
    po_out.read_line(&mut po_line).unwrap();
    let hello_other: VoiceHello = serde_json::from_str(&po_line).unwrap();
    assert_ne!(hello_other.session, hello1.session);
    assert_ne!(hello_other.mic_port, hello1.mic_port);
    drop(p_other.stdin.take());
    drop(po_out);
    assert!(p_other.wait().unwrap().success());

    // 4. Wrong --token on --resume must NOT kick session 1, and falls back to starting a fresh session.
    let mut p_wrong = Command::new(exe)
        .args([
            "voice",
            "--proto", "1",
            "--port", "0",
            "--resume", &hello1.session,
            "--token", "definitely-wrong-token",
            "--idle-timeout", "0",
        ])
        .env("ROAMTTY_DESKTOP_ROOT", &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut pw_out = BufReader::new(p_wrong.stdout.take().unwrap());
    let mut pw_line = String::new();
    pw_out.read_line(&mut pw_line).unwrap();
    let hello_wrong: VoiceHello = serde_json::from_str(&pw_line).unwrap();
    assert_ne!(hello_wrong.session, hello1.session);
    assert_ne!(hello_wrong.mic_port, hello1.mic_port);
    drop(p_wrong.stdin.take());
    drop(pw_out);
    assert!(p_wrong.wait().unwrap().success());

    // 5. Disconnect session 1 (close stdin) -> process exits cleanly, daemon keeps mic_port bound!
    drop(p1.stdin.take());
    drop(p1_out);
    assert!(p1.wait().unwrap().success());

    // Confirm 127.0.0.1:<mic_port> is STILL bound while no client is attached.
    assert!(TcpListener::bind(("127.0.0.1", hello1.mic_port)).is_err(), "daemon must keep mic_port bound");

    // 6. Reconnect with --resume <session> --token <resume_token> and --idle-timeout 0 (so daemon exits after test).
    let mut p2 = Command::new(exe)
        .args([
            "voice",
            "--proto", "1",
            "--resume", &hello1.session,
            "--token", &hello1.resume_token,
            "--idle-timeout", "0",
        ])
        .env("ROAMTTY_DESKTOP_ROOT", &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut p2_in = p2.stdin.take().unwrap();
    let mut p2_out = BufReader::new(p2.stdout.take().unwrap());
    let mut hello2_line = String::new();
    p2_out.read_line(&mut hello2_line).unwrap();
    let hello2: VoiceHello = serde_json::from_str(&hello2_line).unwrap();
    assert_eq!(hello2.mic_port, hello1.mic_port, "--resume must preserve exact mic_port");
    assert_eq!(hello2.session, hello1.session);
    assert_eq!(hello2.resume_token, hello1.resume_token);

    // 7. Exercise real speaker TCP roundtrip through resumed subprocess!
    let mut agy_spk = TcpStream::connect(("127.0.0.1", hello2.mic_port)).unwrap();
    agy_spk.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    agy_spk.write_all(b"JETSKI-AUDIO/1 speaker rate=24000\n").unwrap();
    let (t, p) = proto::read_frame(&mut p2_out).unwrap().unwrap();
    assert_eq!(t, ty::SPK_START);
    let spk_id = u32::from_le_bytes(p[0..4].try_into().unwrap());
    let (at, ap) = proto::encode_client(&ClientMsg::SpkAck { stream_id: spk_id, ok: true, error: String::new() });
    proto::write_frame(&mut p2_in, at, &ap).unwrap();
    p2_in.flush().unwrap();
    let mut ok = [0u8; 19];
    agy_spk.read_exact(&mut ok).unwrap();
    assert_eq!(&ok, b"\nJETSKI-AUDIO/1 OK\n");
    send_jetski_frame(&mut agy_spk, b'A', &[1, 2, 3, 4]);
    let (t, p) = proto::read_frame(&mut p2_out).unwrap().unwrap();
    assert_eq!(t, ty::SPK_AUDIO);
    assert_eq!(&p[4..], &[1, 2, 3, 4]);
    send_jetski_frame(&mut agy_spk, b'C', &[]);
    let (t, _) = proto::read_frame(&mut p2_out).unwrap().unwrap();
    assert_eq!(t, ty::SPK_CLEAR);
    drop(agy_spk);
    let (t, _) = proto::read_frame(&mut p2_out).unwrap().unwrap();
    assert_eq!(t, ty::SPK_STOP);

    // 8. Exercise real mic TCP roundtrip + FIN half-close through resumed subprocess!
    let mut agy_mic = TcpStream::connect(("127.0.0.1", hello2.mic_port)).unwrap();
    agy_mic.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let (t, p) = proto::read_frame(&mut p2_out).unwrap().unwrap();
    assert_eq!(t, ty::MIC_START);
    let mic_id = u32::from_le_bytes(p[0..4].try_into().unwrap());
    let pcm = vec![0x5Au8; 640];
    let (mt, mp) = proto::encode_client(&ClientMsg::MicAudio { stream_id: mic_id, pcm: pcm.clone() });
    proto::write_frame(&mut p2_in, mt, &mp).unwrap();
    p2_in.flush().unwrap();
    let mut got_pcm = vec![0u8; 640];
    agy_mic.read_exact(&mut got_pcm).unwrap();
    assert_eq!(got_pcm, pcm);
    agy_mic.shutdown(Shutdown::Write).unwrap();
    let mut rest = Vec::new();
    agy_mic.read_to_end(&mut rest).unwrap();
    assert!(rest.is_empty());
    let (t, p) = proto::read_frame(&mut p2_out).unwrap().unwrap();
    assert_eq!(t, ty::MIC_STOP);
    assert_eq!(p, mic_id.to_le_bytes());

    // 9. Clean teardown: closing p2 ends daemon immediately (idle_timeout=0).
    drop(p2_in);
    drop(p2_out);
    assert!(p2.wait().unwrap().success());

    // Wait briefly for daemon to exit and release mic_port & cleanup socket.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if TcpListener::bind(("127.0.0.1", hello2.mic_port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(TcpListener::bind(("127.0.0.1", hello2.mic_port)).is_ok(), "daemon should exit after idle_timeout=0");
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn cli_voice_unix_socket_ipc_resume_on_short_root() {
    let exe = env!("CARGO_BIN_EXE_roamtty-desktop");
    let root = std::path::PathBuf::from(format!("/tmp/rvu-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    let mut p1 = Command::new(exe)
        .args(["voice", "--proto", "1", "--port", "0", "--idle-timeout", "10"])
        .env("ROAMTTY_DESKTOP_ROOT", &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut out1 = BufReader::new(p1.stdout.take().unwrap());
    let mut line1 = String::new();
    out1.read_line(&mut line1).unwrap();
    let h1: VoiceHello = serde_json::from_str(&line1).unwrap();
    assert!(root.join("desktop/voice").join(format!("{}.sock", h1.session)).exists(), "expected Unix socket on short root");

    drop(p1.stdin.take());
    drop(out1);
    assert!(p1.wait().unwrap().success());

    let mut p2 = Command::new(exe)
        .args(["voice", "--proto", "1", "--resume", &h1.session, "--token", &h1.resume_token, "--idle-timeout", "0"])
        .env("ROAMTTY_DESKTOP_ROOT", &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut out2 = BufReader::new(p2.stdout.take().unwrap());
    let mut line2 = String::new();
    out2.read_line(&mut line2).unwrap();
    let h2: VoiceHello = serde_json::from_str(&line2).unwrap();
    assert_eq!(h2.mic_port, h1.mic_port);
    assert_eq!(h2.session, h1.session);

    drop(p2.stdin.take());
    drop(out2);
    assert!(p2.wait().unwrap().success());

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !root.join("desktop/voice").join(format!("{}.sock", h1.session)).exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(!root.join("desktop/voice").join(format!("{}.sock", h1.session)).exists(), "sock should be cleaned up on daemon exit");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn cli_voice_explicit_shutdown_releases_port_immediately() {
    let exe = env!("CARGO_BIN_EXE_roamtty-desktop");
    let root = std::env::temp_dir().join(format!("roamtty-voice-shutdown-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    // Pick a free loopback port to act like default port 24713.
    let probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let target_port = probe.local_addr().unwrap().port();
    drop(probe);

    for round in 0..3 {
        let mut proc = Command::new(exe)
            .args(["voice", "--proto", "1", "--port", &target_port.to_string(), "--idle-timeout", "300"])
            .env("ROAMTTY_DESKTOP_ROOT", &root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = proc.stdin.take().unwrap();
        let mut stdout = BufReader::new(proc.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let hello: VoiceHello = serde_json::from_str(&line).unwrap();
        assert_eq!(hello.mic_port, target_port, "round {round} must bind exact preferred port");

        // Send explicit hangup shutdown config -> daemon must exit immediately despite idle_timeout=300!
        let (t, p) = proto::encode_client(&ClientMsg::Config(proto::Config {
            shutdown: Some(true),
            ..Default::default()
        }));
        proto::write_frame(&mut stdin, t, &p).unwrap();
        stdin.flush().unwrap();
        drop(stdin);
        drop(stdout);
        assert!(proc.wait().unwrap().success());

        // Port must become bindable almost immediately (well under 1s, not 300s).
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut freed = false;
        while Instant::now() < deadline {
            if TcpListener::bind(("127.0.0.1", target_port)).is_ok() {
                freed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        assert!(freed, "round {round}: explicit shutdown must release port {target_port} immediately");
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn cli_voice_fresh_start_evicts_idle_daemon_and_reclaims_port() {
    let exe = env!("CARGO_BIN_EXE_roamtty-desktop");
    let root = std::env::temp_dir().join(format!("roamtty-voice-evict-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    let probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let target_port = probe.local_addr().unwrap().port();
    drop(probe);

    // Plant a stale orphaned token+ipc file to verify cleanup as well.
    let vdir = root.join("desktop/voice");
    std::fs::create_dir_all(&vdir).unwrap();
    std::fs::write(vdir.join("deadbeef0000.token"), "stale-token").unwrap();
    std::fs::write(vdir.join("deadbeef0000.ipc"), "1").unwrap();

    // 1. Start session 1 on target_port with 300s idle timeout, then drop stdin WITHOUT shutdown.
    let mut p1 = Command::new(exe)
        .args(["voice", "--proto", "1", "--port", &target_port.to_string(), "--idle-timeout", "300"])
        .env("ROAMTTY_DESKTOP_ROOT", &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut out1 = BufReader::new(p1.stdout.take().unwrap());
    let mut line1 = String::new();
    out1.read_line(&mut line1).unwrap();
    let h1: VoiceHello = serde_json::from_str(&line1).unwrap();
    assert_eq!(h1.mic_port, target_port);
    drop(p1.stdin.take());
    drop(out1);
    assert!(p1.wait().unwrap().success());

    // Confirm old daemon is still squatting on target_port while disconnected.
    assert!(TcpListener::bind(("127.0.0.1", target_port)).is_err());

    // 2. Start a FRESH session requesting the exact same target_port (no --resume).
    //    It MUST evict the idle daemon and reclaim target_port instead of falling back!
    let mut p2 = Command::new(exe)
        .args(["voice", "--proto", "1", "--port", &target_port.to_string(), "--idle-timeout", "0"])
        .env("ROAMTTY_DESKTOP_ROOT", &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut in2 = p2.stdin.take().unwrap();
    let mut out2 = BufReader::new(p2.stdout.take().unwrap());
    let mut line2 = String::new();
    out2.read_line(&mut line2).unwrap();
    let h2: VoiceHello = serde_json::from_str(&line2).unwrap();
    assert_ne!(h2.session, h1.session);
    assert_eq!(h2.mic_port, target_port, "fresh start must evict idle daemon and reclaim target_port");

    // Verify stale files were also cleaned up.
    assert!(!vdir.join("deadbeef0000.token").exists(), "stale token should be cleaned up");

    // Verify agy speaker connection now reaches session 2 (not the evicted zombie)!
    let mut agy = TcpStream::connect(("127.0.0.1", target_port)).unwrap();
    agy.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    agy.write_all(b"JETSKI-AUDIO/1 speaker rate=24000\n").unwrap();
    let (t, p) = proto::read_frame(&mut out2).unwrap().unwrap();
    assert_eq!(t, ty::SPK_START);
    let spk_id = u32::from_le_bytes(p[0..4].try_into().unwrap());
    let (at, ap) = proto::encode_client(&ClientMsg::SpkAck { stream_id: spk_id, ok: true, error: String::new() });
    proto::write_frame(&mut in2, at, &ap).unwrap();
    in2.flush().unwrap();
    let mut ok = [0u8; 19];
    agy.read_exact(&mut ok).unwrap();
    assert_eq!(&ok, b"\nJETSKI-AUDIO/1 OK\n");

    drop(agy);
    drop(in2);
    drop(out2);
    assert!(p2.wait().unwrap().success());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn cli_voice_concurrent_fresh_startups_on_same_port() {
    let exe = env!("CARGO_BIN_EXE_roamtty-desktop");
    let root = std::env::temp_dir().join(format!("roamtty-voice-concurrent-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    let probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let target_port = probe.local_addr().unwrap().port();
    drop(probe);

    let count = 4usize;
    let mut handles = Vec::with_capacity(count);
    for _ in 0..count {
        let root = root.clone();
        handles.push(std::thread::spawn(move || {
            let mut proc = Command::new(exe)
                .args(["voice", "--proto", "1", "--port", &target_port.to_string(), "--idle-timeout", "0"])
                .env("ROAMTTY_DESKTOP_ROOT", &root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let mut stdout = BufReader::new(proc.stdout.take().unwrap());
            let mut line = String::new();
            stdout.read_line(&mut line).unwrap();
            let hello: VoiceHello = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("expected valid VoiceHello, got {line:?} ({e})"));
            (proc, stdout, hello)
        }));
    }

    let mut results = Vec::with_capacity(count);
    for h in handles {
        results.push(h.join().unwrap());
    }

    let mut sessions = std::collections::HashSet::new();
    let mut ports = std::collections::HashSet::new();
    let mut won_target = 0usize;
    for (_, _, hello) in &results {
        assert!(sessions.insert(hello.session.clone()), "duplicate session id");
        assert!(ports.insert(hello.mic_port), "duplicate mic_port {}", hello.mic_port);
        if hello.mic_port == target_port {
            won_target += 1;
        }
    }
    assert_eq!(won_target, 1, "exactly one concurrent fresh startup must win target_port {target_port}");

    for (mut proc, stdout, _) in results {
        drop(proc.stdin.take());
        drop(stdout);
        assert!(proc.wait().unwrap().success());
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn cli_voice_explicit_shutdown_rapid_reconnect_without_polling() {
    let exe = env!("CARGO_BIN_EXE_roamtty-desktop");
    let root = std::env::temp_dir().join(format!("roamtty-voice-rapid-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    let probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let target_port = probe.local_addr().unwrap().port();
    drop(probe);

    for round in 0..5 {
        let mut proc = Command::new(exe)
            .args(["voice", "--proto", "1", "--port", &target_port.to_string(), "--idle-timeout", "300"])
            .env("ROAMTTY_DESKTOP_ROOT", &root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = proc.stdin.take().unwrap();
        let mut stdout = BufReader::new(proc.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let hello: VoiceHello = serde_json::from_str(&line).unwrap();
        assert_eq!(
            hello.mic_port, target_port,
            "round {round}: immediate reconnect without polling must reclaim exact port {target_port}"
        );

        let (t, p) = proto::encode_client(&ClientMsg::Config(proto::Config {
            shutdown: Some(true),
            ..Default::default()
        }));
        proto::write_frame(&mut stdin, t, &p).unwrap();
        stdin.flush().unwrap();
        drop(stdin);
        drop(stdout);
        assert!(proc.wait().unwrap().success());
        // Intentionally NO sleep or TcpListener::bind polling here!
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn cli_voice_codex_backend_isolated_from_agy_and_clean_shutdown() {
    use base64ct::{Base64, Encoding};
    use serde_json::{json, Value};
    use sha1::{Digest, Sha1};
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let exe = env!("CARGO_BIN_EXE_roamtty-desktop");
    let root = std::env::temp_dir().join(format!("roamtty-voice-codex-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    // 1. Invalid --backend returns structured StartError.
    let bad = Command::new(exe)
        .args(["voice", "--proto", "1", "--backend", "nope"])
        .env("ROAMTTY_DESKTOP_ROOT", &root)
        .output()
        .unwrap();
    assert_eq!(bad.status.code(), Some(1));
    let err: StartError = serde_json::from_slice(&bad.stdout).unwrap();
    assert_eq!(err.error, "internal");

    // 2. Start an `agy` daemon first on an ephemeral port and keep it alive (simulates active agy call).
    let mut agy_proc = Command::new(exe)
        .args(["voice", "--proto", "1", "--port", "0", "--idle-timeout", "30"])
        .env("ROAMTTY_DESKTOP_ROOT", &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut agy_stdin = agy_proc.stdin.take().unwrap();
    let mut agy_out = BufReader::new(agy_proc.stdout.take().unwrap());
    let mut agy_line = String::new();
    agy_out.read_line(&mut agy_line).unwrap();
    let agy_hello: VoiceHello = serde_json::from_str(&agy_line).unwrap();
    assert_ne!(agy_hello.mic_port, 0);

    // 3. Spin up a mock Codex app-server Unix WebSocket daemon.
    let sock_path = root.join("mock-codex.sock");
    let listener = UnixListener::bind(&sock_path).unwrap();
    let stop_seen = Arc::new(AtomicBool::new(false));
    let stop_seen_srv = stop_seen.clone();
    let server = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = conn.read(&mut tmp).unwrap();
            buf.extend_from_slice(&tmp[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let req = String::from_utf8_lossy(&buf);
        let key = req
            .lines()
            .find_map(|l| l.strip_prefix("Sec-WebSocket-Key: "))
            .unwrap()
            .trim();
        let mut hasher = Sha1::new();
        hasher.update(key.as_bytes());
        hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
        let accept = Base64::encode_string(&hasher.finalize());
        let resp = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        );
        conn.write_all(resp.as_bytes()).unwrap();

        fn read_ws(conn: &mut std::os::unix::net::UnixStream) -> Option<Value> {
            let mut h = [0u8; 2];
            conn.read_exact(&mut h).ok()?;
            let op = h[0] & 0x0F;
            let mut plen = (h[1] & 0x7F) as usize;
            if plen == 126 {
                let mut b = [0u8; 2];
                conn.read_exact(&mut b).ok()?;
                plen = u16::from_be_bytes(b) as usize;
            } else if plen == 127 {
                let mut b = [0u8; 8];
                conn.read_exact(&mut b).ok()?;
                plen = u64::from_be_bytes(b) as usize;
            }
            let mut mask = [0u8; 4];
            conn.read_exact(&mut mask).ok()?;
            let mut data = vec![0u8; plen];
            conn.read_exact(&mut data).ok()?;
            for (i, b) in data.iter_mut().enumerate() {
                *b ^= mask[i & 3];
            }
            if op == 8 {
                return None;
            }
            serde_json::from_slice(&data).ok()
        }

        fn write_ws(conn: &mut std::os::unix::net::UnixStream, v: &Value) {
            let s = serde_json::to_string(v).unwrap();
            let b = s.as_bytes();
            let mut frame = Vec::new();
            frame.push(0x81);
            if b.len() < 126 {
                frame.push(b.len() as u8);
            } else {
                frame.push(126);
                frame.extend_from_slice(&(b.len() as u16).to_be_bytes());
            }
            frame.extend_from_slice(b);
            let _ = conn.write_all(&frame);
        }

        let mut ans_rtc = str0m::RtcConfig::new()
            .set_crypto_provider(str0m_rust_crypto::default_provider().into())
            .clear_codecs()
            .enable_opus(true, false)
            .build(Instant::now());
        ans_rtc.add_local_candidate(
            str0m::Candidate::host("127.0.0.1:49998".parse().unwrap(), str0m::net::Protocol::Udp)
                .unwrap(),
        );

        while let Some(msg) = read_ws(&mut conn) {
            let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
            let id = msg.get("id").and_then(Value::as_u64);
            match method {
                "initialize" => write_ws(&mut conn, &json!({"id": id, "result": {}})),
                "initialized" => {}
                "thread/loaded/list" => {
                    write_ws(&mut conn, &json!({"id": id, "result": {"data": ["thread-e2e-1"]}}));
                }
                "thread/realtime/start" => {
                    let offer_str = msg["params"]["transport"]["sdp"].as_str().unwrap();
                    let offer = str0m::change::SdpOffer::from_sdp_string(offer_str).unwrap();
                    let answer = ans_rtc.sdp_api().accept_offer(offer).unwrap();
                    write_ws(&mut conn, &json!({"id": id, "result": {}}));
                    write_ws(
                        &mut conn,
                        &json!({
                            "method": "thread/realtime/sdp",
                            "params": { "threadId": "thread-e2e-1", "sdp": answer.to_sdp_string() }
                        }),
                    );
                }
                "thread/realtime/stop" => {
                    stop_seen_srv.store(true, Ordering::SeqCst);
                    write_ws(&mut conn, &json!({"id": id, "result": {}}));
                    break;
                }
                _ => {}
            }
        }
    });

    // 4. Launch Codex voice concurrently with the active `agy` daemon.
    let mut codex_proc = Command::new(exe)
        .args([
            "voice",
            "--proto", "1",
            "--backend", "codex",
            "--codex-thread", "thread-e2e-1",
            "--codex-sock", sock_path.to_str().unwrap(),
            "--idle-timeout", "0",
        ])
        .env("ROAMTTY_DESKTOP_ROOT", &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut codex_stdin = codex_proc.stdin.take().unwrap();
    let mut codex_out = BufReader::new(codex_proc.stdout.take().unwrap());
    let mut codex_line = String::new();
    codex_out.read_line(&mut codex_line).unwrap();
    let codex_hello: VoiceHello = serde_json::from_str(&codex_line).unwrap();
    assert_eq!(codex_hello.mic_port, 0, "codex backend must not bind a loopback TCP mic port");
    assert_eq!(codex_hello.codecs, vec!["opus", "pcm16"]);
    assert!(codex_hello.udp.is_some());

    // 5. Verify the concurrent `agy` daemon's loopback port is STILL bound and untouched!
    assert!(
        TcpListener::bind(("127.0.0.1", agy_hello.mic_port)).is_err(),
        "starting codex voice must never evict or disturb active agy daemon"
    );

    // 6. Send explicit shutdown to codex -> daemon must call thread/realtime/stop and exit cleanly.
    let (t, p) = proto::encode_client(&ClientMsg::Config(proto::Config {
        shutdown: Some(true),
        ..Default::default()
    }));
    proto::write_frame(&mut codex_stdin, t, &p).unwrap();
    codex_stdin.flush().unwrap();
    drop(codex_stdin);
    drop(codex_out);
    assert!(codex_proc.wait().unwrap().success());
    server.join().unwrap();
    assert!(stop_seen.load(Ordering::SeqCst), "codex shutdown must invoke thread/realtime/stop");

    // 7. Cleanly shut down the `agy` daemon as well.
    proto::write_frame(&mut agy_stdin, t, &p).unwrap();
    agy_stdin.flush().unwrap();
    drop(agy_stdin);
    drop(agy_out);
    assert!(agy_proc.wait().unwrap().success());
    let _ = std::fs::remove_dir_all(&root);
}
