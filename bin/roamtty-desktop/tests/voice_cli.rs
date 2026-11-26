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
    assert_eq!(hello1.codecs, vec!["pcm16"]);
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
