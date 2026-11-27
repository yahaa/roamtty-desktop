//! Headless `voice` subcommand: bridges remote `agy /voice` (`JETSKI-AUDIO/1` on loopback TCP)
//! over the existing `roamtty-desktop` binary framing and `UdpLink` (with SSH fallback).
//!
//! Zero GUI, AppKit, ScreenCaptureKit, TCC, X11/Wayland, Win32 desktop, or OpenH264 dependencies.

use crate::paths;
use crate::session::{self, Session};
use crate::udp_link::UdpLink;
use crate::voice_codec::{
    frame_samples, i16_to_s16le_bytes, is_opus_sample_rate, pcm_rms, s16le_bytes_to_i16, RedHistory,
    VoiceJitterBuffer, VoiceLinkController, VoiceOpusEncoder,
};
use proto::{ty, voice_flags, ClientMsg, StartError, VoiceHello, VoiceOpusPacket};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const DEFAULT_PORT: u16 = 24713;
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 300;

const PEEK_TIMEOUT: Duration = Duration::from_millis(80);
const HEADER_DEADLINE: Duration = Duration::from_secs(3);
const HEADER_MAX_BYTES: usize = 256;
const SPK_ACK_TIMEOUT: Duration = Duration::from_millis(2500);
const MIC_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const SPK_FRAME_READ_TIMEOUT: Duration = Duration::from_secs(5);
const SPK_FRAME_MAX_LEN: usize = 1024 * 1024;
const MAX_SPK_AUDIO_CHUNK: usize = 960;
const MIC_SAMPLE_RATE: u32 = 16_000;
const CHANNELS: u8 = 1;
const JETSKI_PREFIX: &[u8] = b"JETSKI-AUDIO/1 ";
const SPK_ERR_MAX_BYTES: usize = 235;
const MIN_SAMPLE_RATE: u32 = 8_000;
const MAX_SAMPLE_RATE: u32 = 192_000;
const MIC_QUEUE_MAX_CHUNKS: usize = 15;
const UDP_KEEPALIVE_US: u64 = 30_000_000;
const IPC_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const SSH_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const SSH_QUEUE_CAPACITY: usize = 256;
const SSH_VOICE_AUDIO_CAP: usize = 12;
const SPK_TAIL_FLUSH_MS: u64 = 25;
const VOICE_STATS_INTERVAL_MS: u64 = 250;
const EV_CHANNEL_CAPACITY: usize = 512;
const MAX_EVENT_DRAIN: usize = 64;
const MAX_AGY_CONNS: usize = 32;
const EXIT_BIND_FAILED: i32 = 10;

fn voice_dir() -> PathBuf {
    paths::desktop_dir().join("voice")
}

fn is_valid_session_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn print_start_error(code: &str, detail: Option<String>, min: Option<u32>, max: Option<u32>) -> i32 {
    let e = StartError { error: code.into(), detail, min, max };
    println!("{}", serde_json::to_string(&e).unwrap());
    let _ = io::stdout().flush();
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttachReq {
    ipc_token: String,
    proto: u32,
    resume: Option<String>,
    token: Option<String>,
    idle_timeout: Option<u64>,
    ssh_client: Option<String>,
    #[serde(default)]
    evict_idle_port: Option<u16>,
    #[serde(default)]
    shutdown: bool,
}

/// Entry point for `roamtty-desktop voice --proto 1 [--port P] [--resume S --token T] [--idle-timeout SEC]`.
pub fn cmd(args: &[String]) -> i32 {
    let proto_ver = match flag(args, "--proto") {
        Some(p) => match p.parse::<u32>() {
            Ok(v) => v,
            Err(_) => return print_start_error("proto_unsupported", Some("invalid --proto".into()), Some(proto::PROTO), Some(proto::PROTO)),
        },
        None => proto::PROTO,
    };
    if proto_ver != proto::PROTO {
        return print_start_error("proto_unsupported", None, Some(proto::PROTO), Some(proto::PROTO));
    }
    let port = match flag(args, "--port") {
        Some(p) => match p.parse::<u16>() {
            Ok(v) => v,
            Err(_) => return print_start_error("internal", Some("invalid --port".into()), None, None),
        },
        None => DEFAULT_PORT,
    };
    let idle_timeout_arg = match flag(args, "--idle-timeout") {
        Some(t) => match t.parse::<u64>() {
            Ok(v) => Some(v),
            Err(_) => return print_start_error("internal", Some("invalid --idle-timeout".into()), None, None),
        },
        None => None,
    };
    let resume = flag(args, "--resume").map(String::from);
    let token = flag(args, "--token").map(String::from);
    let ssh_client = std::env::var("SSH_CLIENT").ok();

    // 1. If --resume <S> --token <T> was supplied, attempt takeover on that session's daemon first.
    if let (Some(ref sess), Some(ref tok)) = (&resume, &token) {
        if is_valid_session_id(sess) {
            if let Ok(conn) = try_attach_existing(sess, proto_ver, Some(sess.clone()), Some(tok.clone()), idle_timeout_arg, ssh_client.clone(), true) {
                return relay_to_stdio(conn);
            }
        }
    }

    // 2. Otherwise (fresh start, expired daemon, or unauthorized resume token): evict any idle/disconnected
    //    daemon still holding the preferred port (and clean up stale IPC files), then spawn a fresh daemon.
    if port != 0 {
        evict_idle_daemons_on_port(port);
    }
    let new_session = session::hex(&session::random_bytes(8));
    let idle_secs = idle_timeout_arg.unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS);
    match spawn_and_attach(&new_session, proto_ver, port, idle_secs, ssh_client) {
        Ok(conn) => relay_to_stdio(conn),
        Err((code, detail)) => print_start_error(code, Some(detail), None, None),
    }
}

fn list_voice_sessions() -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(voice_dir()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name();
        let Some(s) = name.to_str() else { continue };
        if let Some(sess) = s.strip_suffix(".token") {
            if is_valid_session_id(sess) {
                out.push(sess.to_string());
            }
        }
    }
    out
}

fn is_stale_ipc_error(e: &io::Error) -> bool {
    // On Windows, connecting to an unused loopback TCP port takes ~1s to return WSAECONNREFUSED,
    // so our 300ms loopback connect_timeout surfaces TimedOut for dead IPC ports.
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::NotFound
            | io::ErrorKind::TimedOut
            | io::ErrorKind::InvalidData
    )
}

fn wait_for_session_exit(sess: &str, max_wait: Duration) {
    let deadline = Instant::now() + max_wait;
    while Instant::now() < deadline {
        if ipc::read_token(sess).is_none() {
            return;
        }
        match ipc::connect(sess) {
            Ok(c) => c.close(),
            Err(e) if is_stale_ipc_error(&e) => {
                ipc::cleanup(sess);
                return;
            }
            Err(_) => return,
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn evict_idle_daemons_on_port(port: u16) {
    for sess in list_voice_sessions() {
        let Some(ipc_token) = ipc::read_token(&sess) else { continue };
        let mut conn = match ipc::connect(&sess) {
            Ok(c) => c,
            Err(e) => {
                if is_stale_ipc_error(&e) {
                    ipc::cleanup(&sess);
                }
                continue;
            }
        };
        conn.set_io_timeout(Some(Duration::from_millis(500)));
        let req = AttachReq {
            ipc_token: ipc_token.trim().to_string(),
            proto: proto::PROTO,
            resume: None,
            token: None,
            idle_timeout: None,
            ssh_client: None,
            evict_idle_port: Some(port),
            shutdown: false,
        };
        let reply = if writeln!(conn, "{}", serde_json::to_string(&req).unwrap()).is_ok() && conn.flush().is_ok() {
            read_line_unbuffered(&mut *conn, 64).ok()
        } else {
            None
        };
        conn.close();
        if reply.as_deref().is_some_and(|r| r.trim() == "ERR shutting down") {
            wait_for_session_exit(&sess, Duration::from_millis(200));
        }
    }
}

/// Gracefully shuts down all running voice daemons (used by `uninstall`).
pub fn stop_all_daemons() {
    for sess in list_voice_sessions() {
        let Some(ipc_token) = ipc::read_token(&sess) else { continue };
        let mut conn = match ipc::connect(&sess) {
            Ok(c) => c,
            Err(e) => {
                if is_stale_ipc_error(&e) {
                    ipc::cleanup(&sess);
                }
                continue;
            }
        };
        conn.set_io_timeout(Some(Duration::from_millis(500)));
        let req = AttachReq {
            ipc_token: ipc_token.trim().to_string(),
            proto: proto::PROTO,
            resume: None,
            token: None,
            idle_timeout: None,
            ssh_client: None,
            evict_idle_port: None,
            shutdown: true,
        };
        let reply = if writeln!(conn, "{}", serde_json::to_string(&req).unwrap()).is_ok() && conn.flush().is_ok() {
            read_line_unbuffered(&mut *conn, 64).ok()
        } else {
            None
        };
        conn.close();
        if reply.as_deref().is_none_or(|r| r.trim() == "ERR shutting down") {
            wait_for_session_exit(&sess, Duration::from_millis(200));
        }
    }
}

/// Internal entry point for the detached background daemon:
/// `roamtty-desktop voice-daemon --session <S> --port <P> --idle-timeout <SEC>`.
/// No secrets are passed on argv; tokens are generated internally and written mode 0600.
pub fn daemon_cmd(args: &[String]) -> i32 {
    let Some(sess_id) = flag(args, "--session").map(String::from) else {
        return 2;
    };
    if !is_valid_session_id(&sess_id) {
        return 2;
    }
    let port = flag(args, "--port").and_then(|p| p.parse::<u16>().ok()).unwrap_or(DEFAULT_PORT);
    let idle_secs = flag(args, "--idle-timeout").and_then(|t| t.parse::<u64>().ok()).unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS);

    if let Err(e) = paths::ensure(&voice_dir()) {
        crate::log!("voice-daemon dir error: {e}");
        return 1;
    }
    let (mic_listener, mic_port) = match bind_mic_listener(port) {
        Ok(v) => v,
        Err(e) => {
            crate::log!("voice-daemon bind failed: {e}");
            return EXIT_BIND_FAILED;
        }
    };
    let ipc_token = session::random_b64(24);
    let session = Session {
        session: sess_id.clone(),
        resume_token: session::random_b64(16),
    };
    let shutdown = Arc::new(AtomicBool::new(false));
    let ipc_listener = match ipc::listen(&sess_id, &ipc_token, shutdown.clone()) {
        Ok(l) => l,
        Err(e) => {
            crate::log!("voice-daemon ipc listen failed: {e}");
            ipc::cleanup(&sess_id);
            return 1;
        }
    };
    crate::log!("voice-daemon start session={sess_id} mic_port={mic_port} idle={idle_secs}s");

    let (ev_tx, ev_rx) = mpsc::sync_channel::<Event>(EV_CHANNEL_CAPACITY);
    let client_connected = Arc::new(AtomicBool::new(false));
    let next_stream_id = Arc::new(AtomicU32::new(1));

    let mic_accept = spawn_mic_accept_loop(
        mic_listener,
        next_stream_id,
        ev_tx.clone(),
        client_connected.clone(),
        shutdown.clone(),
    );
    spawn_ipc_accept_loop(ipc_listener, ipc_token, ev_tx.clone(), shutdown.clone());

    let mut engine = VoiceEngine::new(
        mic_port,
        session,
        Duration::from_secs(idle_secs),
        Box::new(UdpLink::bind),
        ev_tx,
        client_connected,
        shutdown.clone(),
    );
    let exit = engine.run(ev_rx);

    shutdown.store(true, Ordering::SeqCst);
    let _ = mic_accept.join();
    ipc::cleanup(&sess_id);
    if let Some(c) = exit.active_client {
        c.close();
    }
    if let Some((reply_tx, flushed_rx)) = exit.evict_reply {
        let _ = reply_tx.send(true);
        let _ = flushed_rx.recv_timeout(Duration::from_millis(200));
    }
    crate::log!("voice-daemon exit session={sess_id} mic_port={mic_port}");
    0
}

fn try_attach_existing(
    sess_id: &str,
    proto_ver: u32,
    resume: Option<String>,
    token: Option<String>,
    idle_timeout: Option<u64>,
    ssh_client: Option<String>,
    cleanup_stale: bool,
) -> Result<Box<dyn IpcConn>, String> {
    let ipc_token = ipc::read_token(sess_id).ok_or_else(|| "missing token".to_string())?;
    let mut conn = ipc::connect(sess_id).map_err(|e| {
        // Stale socket/ipc file from a dead daemon: clean up quietly only when probing an existing session.
        if cleanup_stale && is_stale_ipc_error(&e) {
            ipc::cleanup(sess_id);
        }
        e.to_string()
    })?;
    conn.set_io_timeout(Some(IPC_HANDSHAKE_TIMEOUT));
    let req = AttachReq {
        ipc_token: ipc_token.trim().to_string(),
        proto: proto_ver,
        resume,
        token,
        idle_timeout,
        ssh_client,
        evict_idle_port: None,
        shutdown: false,
    };
    if let Err(e) = writeln!(conn, "{}", serde_json::to_string(&req).unwrap()).and_then(|_| conn.flush()) {
        conn.close();
        return Err(e.to_string());
    }
    let status = match read_line_unbuffered(&mut *conn, 64) {
        Ok(s) => s,
        Err(e) => {
            conn.close();
            return Err(e.to_string());
        }
    };
    if status.trim() == "OK" {
        conn.set_io_timeout(None);
        Ok(conn)
    } else {
        conn.close();
        Err(format!("attach rejected: {}", status.trim()))
    }
}

fn spawn_and_attach(
    sess_id: &str,
    proto_ver: u32,
    port: u16,
    idle_secs: u64,
    ssh_client: Option<String>,
) -> Result<Box<dyn IpcConn>, (&'static str, String)> {
    let exe = std::env::current_exe().map_err(|e| ("internal", e.to_string()))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args([
        "voice-daemon",
        "--session",
        sess_id,
        "--port",
        &port.to_string(),
        "--idle-timeout",
        &idle_secs.to_string(),
    ])
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    let mut child = cmd.spawn().map_err(|e| ("internal", format!("spawn voice-daemon: {e}")))?;
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            let code = status.code().unwrap_or(1);
            if code == EXIT_BIND_FAILED {
                return Err(("bind_failed", format!("failed to bind loopback voice port (requested {port})")));
            }
            return Err(("internal", format!("voice-daemon exited early ({code})")));
        }
        if let Ok(conn) = try_attach_existing(sess_id, proto_ver, None, None, Some(idle_secs), ssh_client.clone(), false) {
            return Ok(conn);
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    let _ = child.kill();
    let _ = child.wait();
    ipc::cleanup(sess_id);
    Err(("internal", "voice-daemon did not become ready in time".into()))
}

/// Reads a single `\n`-terminated ASCII line byte-by-byte so zero bytes following `\n` are lost.
fn read_line_unbuffered(r: &mut dyn Read, max_len: usize) -> io::Result<String> {
    let mut buf = Vec::with_capacity(16);
    let mut b = [0u8; 1];
    loop {
        match r.read(&mut b) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof before newline")),
            Ok(_) => {
                buf.push(b[0]);
                if b[0] == b'\n' {
                    break;
                }
                if buf.len() >= max_len {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    String::from_utf8(buf).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid utf-8"))
}

fn relay_to_stdio(conn: Box<dyn IpcConn>) -> i32 {
    if relay(conn, io::stdin(), io::stdout()).is_err() {
        return 1;
    }
    0
}

fn relay(mut conn: Box<dyn IpcConn>, mut input: impl Read + Send + 'static, mut output: impl Write + Send + 'static) -> io::Result<()> {
    let mut back = conn.split()?;
    let closer = conn.split()?;
    enum RelayEnd {
        Up,
        Down,
    }
    let (done, ended) = mpsc::channel();
    let up = done.clone();
    std::thread::spawn(move || {
        let _ = io::copy(&mut input, &mut conn);
        let _ = conn.flush();
        conn.shutdown_write();
        let _ = up.send(RelayEnd::Up);
    });
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 1 << 16];
        loop {
            match back.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if output.write_all(&buf[..n]).and_then(|_| output.flush()).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let _ = done.send(RelayEnd::Down);
    });
    match ended.recv() {
        Ok(RelayEnd::Up) => {
            // Allow the daemon to finish processing any final frame (e.g. `CONFIG {"shutdown":true}`)
            // and close the IPC stream cleanly before forcing Shutdown::Both, preventing TCP RST.
            let _ = ended.recv_timeout(Duration::from_millis(250));
        }
        Ok(RelayEnd::Down) | Err(_) => {}
    }
    closer.close();
    Ok(())
}

// ============================================================================
// Bind & JETSKI-AUDIO/1 protocol helpers
// ============================================================================

pub fn bind_mic_listener(preferred_port: u16) -> io::Result<(TcpListener, u16)> {
    let listener = if preferred_port != 0 {
        match TcpListener::bind(("127.0.0.1", preferred_port)) {
            Ok(l) => l,
            Err(e) => {
                crate::log!("voice port {preferred_port} busy ({e}), falling back to ephemeral port");
                TcpListener::bind(("127.0.0.1", 0))?
            }
        }
    } else {
        TcpListener::bind(("127.0.0.1", 0))?
    };
    let port = listener.local_addr()?.port();
    Ok((listener, port))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeekClass {
    Speaker,
    Eof,
    Invalid,
}

fn classify_peek_bytes(bytes: &[u8]) -> PeekClass {
    if bytes.is_empty() {
        return PeekClass::Eof;
    }
    if bytes.len() >= JETSKI_PREFIX.len() {
        if &bytes[..JETSKI_PREFIX.len()] == JETSKI_PREFIX {
            PeekClass::Speaker
        } else {
            PeekClass::Invalid
        }
    } else if JETSKI_PREFIX.starts_with(bytes) {
        PeekClass::Speaker
    } else {
        PeekClass::Invalid
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnKind {
    Mic,
    Speaker,
    Eof,
    Invalid,
}

fn classify_connection(stream: &TcpStream) -> ConnKind {
    let deadline = Instant::now() + PEEK_TIMEOUT;
    let mut buf = [0u8; HEADER_MAX_BYTES];
    loop {
        let Some(rem) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else {
            return ConnKind::Mic;
        };
        if stream.set_read_timeout(Some(rem)).is_err() {
            return ConnKind::Eof;
        }
        match stream.peek(&mut buf) {
            Ok(0) => return ConnKind::Eof,
            Ok(n) => {
                return match classify_peek_bytes(&buf[..n]) {
                    PeekClass::Speaker => ConnKind::Speaker,
                    PeekClass::Eof => ConnKind::Eof,
                    PeekClass::Invalid => ConnKind::Invalid,
                };
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
                return ConnKind::Mic;
            }
            Err(_) => return ConnKind::Eof,
        }
    }
}

fn parse_speaker_header(line: &str) -> Result<u32, &'static str> {
    let trimmed = line.strip_suffix('\n').unwrap_or(line);
    let trimmed = trimmed.strip_suffix('\r').unwrap_or(trimmed);
    let rest = trimmed.strip_prefix("JETSKI-AUDIO/1 ").ok_or("invalid protocol prefix")?;
    let mut parts = rest.split_ascii_whitespace();
    match parts.next() {
        Some("speaker") => {}
        _ => return Err("expected speaker role"),
    }
    let mut rate: Option<u32> = None;
    let mut channels: Option<u8> = None;
    for tok in parts {
        if let Some(val) = tok.strip_prefix("rate=") {
            if rate.is_some() {
                return Err("duplicate rate");
            }
            let parsed = val.parse::<u32>().map_err(|_| "invalid sample rate")?;
            if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&parsed) {
                return Err("sample rate out of range");
            }
            rate = Some(parsed);
        } else if let Some(val) = tok.strip_prefix("channels=") {
            if channels.is_some() {
                return Err("duplicate channels");
            }
            let parsed = val.parse::<u8>().map_err(|_| "invalid channels")?;
            if parsed != CHANNELS {
                return Err("unsupported channel count");
            }
            channels = Some(parsed);
        }
    }
    rate.ok_or("missing sample rate")
}

fn sanitize_err_reason(err: &str) -> String {
    let cleaned: String = err
        .chars()
        .map(|c| if c == '\r' || c == '\n' { ' ' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return "speaker error".into();
    }
    if trimmed.len() <= SPK_ERR_MAX_BYTES {
        return trimmed.to_string();
    }
    let mut end = SPK_ERR_MAX_BYTES;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    let out = trimmed[..end].trim_end();
    if out.is_empty() {
        "speaker error".into()
    } else {
        out.to_string()
    }
}

fn format_spk_ok() -> &'static [u8] {
    b"\nJETSKI-AUDIO/1 OK\n"
}

fn format_spk_err(reason: &str) -> Vec<u8> {
    format!("\nJETSKI-AUDIO/1 ERR {}\n", sanitize_err_reason(reason)).into_bytes()
}

fn read_speaker_header(reader: &mut BufReader<TcpStream>, shutdown: &AtomicBool) -> Result<u32, &'static str> {
    let deadline = Instant::now() + HEADER_DEADLINE;
    let mut line = Vec::with_capacity(64);
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return Err("daemon shutting down");
        }
        let Some(rem) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else {
            return Err("header timeout");
        };
        let _ = reader.get_ref().set_read_timeout(Some(rem.min(Duration::from_millis(100))));
        match reader.fill_buf() {
            Ok(&[]) => return Err("unexpected eof"),
            Ok(avail) => {
                if let Some(nl) = avail.iter().position(|&b| b == b'\n') {
                    let take = nl + 1;
                    if line.len() + take > HEADER_MAX_BYTES {
                        reader.consume(take);
                        return Err("header too long");
                    }
                    line.extend_from_slice(&avail[..take]);
                    reader.consume(take);
                    break;
                }
                if line.len() + avail.len() >= HEADER_MAX_BYTES {
                    let n = avail.len();
                    reader.consume(n);
                    return Err("header too long");
                }
                let n = avail.len();
                line.extend_from_slice(avail);
                reader.consume(n);
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted) => continue,
            Err(_) => return Err("header read error"),
        }
    }
    let s = std::str::from_utf8(&line).map_err(|_| "invalid utf-8 header")?;
    parse_speaker_header(s)
}

/// Reads exactly `dst.len()` bytes from `reader`, polling `stop`/`shutdown` between short timeouts.
/// Returns `Ok(true)` on full read, `Ok(false)` on clean EOF before any byte when `in_frame_deadline`
/// is `None` (between frames) or on stop/shutdown, or `Err` on truncated EOF / deadline / I/O error.
fn read_exact_interruptible(
    reader: &mut BufReader<TcpStream>,
    dst: &mut [u8],
    stop: &AtomicBool,
    shutdown: &AtomicBool,
    in_frame_deadline: Option<Instant>,
) -> io::Result<bool> {
    let mut off = 0usize;
    let mut mid_deadline = in_frame_deadline;
    while off < dst.len() {
        if stop.load(Ordering::Relaxed) || shutdown.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let timeout = if let Some(dl) = mid_deadline {
            let Some(rem) = dl.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "frame read timeout"));
            };
            rem.min(Duration::from_millis(100))
        } else {
            Duration::from_millis(150)
        };
        let _ = reader.get_ref().set_read_timeout(Some(timeout));
        match reader.read(&mut dst[off..]) {
            Ok(0) => {
                if off == 0 && in_frame_deadline.is_none() {
                    return Ok(false);
                }
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated frame"));
            }
            Ok(n) => {
                if off == 0 && mid_deadline.is_none() {
                    mid_deadline = Some(Instant::now() + SPK_FRAME_READ_TIMEOUT);
                }
                off += n;
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

// ============================================================================
// Core Voice Engine (transport-agnostic & directly testable)
// ============================================================================

struct MicQueue {
    chunks: VecDeque<Vec<u8>>,
    closed: bool,
}

struct MicStreamHandle {
    queue: Arc<(Mutex<MicQueue>, Condvar)>,
    opus_jb: Arc<Mutex<Option<VoiceJitterBuffer>>>,
    sock: TcpStream,
    announced_to_epoch: Option<u64>,
}

impl MicStreamHandle {
    fn push(&self, pcm: Vec<u8>) {
        let (lock, cvar) = &*self.queue;
        let mut q = lock.lock().unwrap();
        if q.closed {
            return;
        }
        while q.chunks.len() >= MIC_QUEUE_MAX_CHUNKS {
            q.chunks.pop_front();
        }
        q.chunks.push_back(pcm);
        cvar.notify_one();
    }

    fn push_opus(&self, stream_id: u32, pkt: &VoiceOpusPacket, now_ms: u64) {
        let mut guard = self.opus_jb.lock().unwrap();
        let newly_created = guard.is_none();
        let jb = match guard.as_mut() {
            Some(jb) => jb,
            None => match VoiceJitterBuffer::new(stream_id, MIC_SAMPLE_RATE) {
                Ok(created) => guard.insert(created),
                Err(e) => {
                    crate::log!("voice mic {stream_id} opus decoder init failed: {e}");
                    return;
                }
            },
        };
        jb.push(pkt, now_ms);
        drop(guard);
        if newly_created {
            let (lock, cvar) = &*self.queue;
            let _q = lock.lock().unwrap();
            cvar.notify_one();
        }
    }

    fn stats_snapshot(&self, now_ms: u64) -> Option<proto::VoiceStats> {
        let mut guard = self.opus_jb.lock().unwrap();
        guard.as_mut().map(|jb| jb.stats_snapshot(now_ms))
    }

    fn close(&self) {
        let (lock, cvar) = &*self.queue;
        {
            let mut q = lock.lock().unwrap();
            q.closed = true;
            q.chunks.clear();
        }
        cvar.notify_all();
        let _ = self.sock.shutdown(Shutdown::Both);
    }
}

const MAX_SPK_OPUS_TX_QUEUE: usize = 250;
const SPK_OPUS_PACE_MS: u64 = 19;

struct SpkOpusState {
    encoder: VoiceOpusEncoder,
    red: RedHistory,
    pcm_pending: Vec<i16>,
    scratch: Vec<i16>,
    last_pcm_at: Option<Instant>,
    tx_queue: VecDeque<VoiceOpusPacket>,
    next_tx_at: Option<Instant>,
    burst_budget: u8,
    seq: u32,
    turn_gen: u8,
    pending_reset: bool,
}

impl SpkOpusState {
    fn new(sample_rate: u32, initial_bitrate: i32) -> Result<Self, String> {
        Ok(Self {
            encoder: VoiceOpusEncoder::new(sample_rate, initial_bitrate, false)?,
            red: RedHistory::default(),
            pcm_pending: Vec::with_capacity(frame_samples(sample_rate) * 2),
            scratch: Vec::with_capacity(frame_samples(sample_rate) * 2),
            last_pcm_at: None,
            tx_queue: VecDeque::with_capacity(64),
            next_tx_at: None,
            burst_budget: 2,
            seq: 0,
            turn_gen: 0,
            pending_reset: true,
        })
    }

    fn clear(&mut self) {
        self.pcm_pending.clear();
        self.last_pcm_at = None;
        self.tx_queue.clear();
        self.next_tx_at = None;
        self.burst_budget = 2;
        self.red.clear();
        self.encoder.reset();
        self.turn_gen = self.turn_gen.wrapping_add(1) & 0x0F;
        self.pending_reset = true;
    }

    fn enqueue_tx(&mut self, pkt: VoiceOpusPacket, now: Instant) {
        if self.tx_queue.is_empty()
            && self.next_tx_at.is_none_or(|t| now >= t + Duration::from_millis(40))
        {
            self.next_tx_at = Some(now);
            self.burst_budget = 2;
        }
        while self.tx_queue.len() >= MAX_SPK_OPUS_TX_QUEUE {
            self.tx_queue.pop_front();
        }
        self.tx_queue.push_back(pkt);
    }

    fn encode_frame(&mut self, stream_id: u32, pcm: &[i16], red_count: usize) -> Option<VoiceOpusPacket> {
        let encoded = match self.encoder.encode(pcm) {
            Ok(b) => b,
            Err(e) => {
                crate::log!("voice spk {stream_id} opus encode failed: {e}");
                return None;
            }
        };
        let seq = self.seq;
        let red = self.red.build_red(seq, red_count);
        let is_speech = pcm_rms(pcm) >= 0.004;
        let mut raw_flags = voice_flags::FEC;
        if is_speech {
            raw_flags |= voice_flags::SPEECH;
        }
        if self.pending_reset {
            raw_flags |= voice_flags::RESET;
            self.pending_reset = false;
        }
        let flags = voice_flags::with_gen(raw_flags, self.turn_gen);
        let ts_samples = seq.wrapping_mul(self.encoder.frame_len() as u32);
        self.red.record(seq, encoded.clone());
        self.seq = seq.wrapping_add(1);
        Some(VoiceOpusPacket {
            stream_id,
            seq,
            ts_samples,
            flags,
            red,
            primary: encoded,
        })
    }
}

struct SpkStreamState {
    sample_rate: u32,
    ack_tx: Option<mpsc::Sender<(bool, String)>>,
    stop_flag: Arc<AtomicBool>,
    announced_to_epoch: Option<u64>,
    opus: Option<SpkOpusState>,
    link_ctrl: VoiceLinkController,
}

enum Event {
    ClientAttach {
        req: AttachReq,
        input: Box<dyn Read + Send>,
        output: Box<dyn Write + Send>,
        closer: Box<dyn Fn() + Send>,
        reply_tx: mpsc::Sender<bool>,
    },
    ClientFrame {
        epoch: u64,
        msg: ClientMsg,
    },
    ClientSshClosed {
        epoch: u64,
    },
    AgyMicOpen {
        stream_id: u32,
        handle: MicStreamHandle,
    },
    AgyMicClosed {
        stream_id: u32,
    },
    AgySpkOpen {
        stream_id: u32,
        sample_rate: u32,
        ack_tx: mpsc::Sender<(bool, String)>,
        stop_flag: Arc<AtomicBool>,
    },
    AgySpkAudio {
        stream_id: u32,
        pcm: Vec<u8>,
    },
    AgySpkClear {
        stream_id: u32,
    },
    AgySpkClosed {
        stream_id: u32,
        notify_client: bool,
    },
    EvictIdle {
        port: u16,
        reply_tx: mpsc::Sender<bool>,
        flushed_rx: mpsc::Receiver<()>,
    },
    ForceShutdown {
        reply_tx: mpsc::Sender<bool>,
        flushed_rx: mpsc::Receiver<()>,
    },
    #[cfg_attr(not(test), allow(dead_code))]
    Shutdown,
}

enum SshOutItem {
    Raw(Vec<u8>),
    Frame { ty: u8, payload: Vec<u8> },
}

impl SshOutItem {
    fn is_droppable_audio(&self) -> bool {
        matches!(
            self,
            SshOutItem::Frame {
                ty: ty::SPK_AUDIO | ty::SPK_OPUS | ty::VOICE_STATS_S2C,
                ..
            }
        )
    }
}

struct SshQueue {
    items: VecDeque<SshOutItem>,
    closed: bool,
    failed: bool,
}

/// Non-blocking bounded writer queue decoupled onto a dedicated thread so `VoiceEngine::run`
/// never blocks in kernel `write()`/`flush()` when an SSH `relay()` stalls.
struct SshWriter {
    queue: Arc<(Mutex<SshQueue>, Condvar)>,
}

impl SshWriter {
    fn spawn(output: Box<dyn Write + Send>, epoch: u64, ev_tx: mpsc::SyncSender<Event>) -> Self {
        let queue = Arc::new((
            Mutex::new(SshQueue {
                items: VecDeque::new(),
                closed: false,
                failed: false,
            }),
            Condvar::new(),
        ));
        let q_clone = queue.clone();
        std::thread::spawn(move || {
            let (lock, cvar) = &*q_clone;
            let mut writer = BufWriter::with_capacity(64 * 1024, output);
            loop {
                let batch: Vec<SshOutItem> = {
                    let mut q = lock.lock().unwrap();
                    while q.items.is_empty() && !q.closed && !q.failed {
                        q = cvar.wait(q).unwrap();
                    }
                    if q.closed || q.failed {
                        break;
                    }
                    q.items.drain(..).collect()
                };
                let mut err = false;
                for item in batch {
                    let res = match item {
                        SshOutItem::Raw(bytes) => writer.write_all(&bytes),
                        SshOutItem::Frame { ty, payload } => proto::write_frame(&mut writer, ty, &payload),
                    };
                    if res.is_err() {
                        err = true;
                        break;
                    }
                }
                if !err && writer.flush().is_err() {
                    err = true;
                }
                if err {
                    {
                        let mut q = lock.lock().unwrap();
                        q.failed = true;
                        q.items.clear();
                    }
                    let _ = ev_tx.send(Event::ClientSshClosed { epoch });
                    break;
                }
            }
        });
        Self { queue }
    }

    /// Enqueues an item without blocking on I/O. Keeps real-time audio capped low on SSH fallback
    /// (evicting oldest audio frames so TCP head-of-line blocking never builds multi-second lag),
    /// while preserving full headroom for control frames.
    fn enqueue(&self, item: SshOutItem) -> bool {
        let (lock, cvar) = &*self.queue;
        let mut q = lock.lock().unwrap();
        if q.closed || q.failed {
            return false;
        }
        if item.is_droppable_audio() {
            let mut audio_count = q.items.iter().filter(|i| i.is_droppable_audio()).count();
            while audio_count >= SSH_VOICE_AUDIO_CAP {
                if let Some(pos) = q.items.iter().position(SshOutItem::is_droppable_audio) {
                    q.items.remove(pos);
                    audio_count -= 1;
                } else {
                    break;
                }
            }
        }
        if q.items.len() >= SSH_QUEUE_CAPACITY {
            if let Some(pos) = q.items.iter().position(SshOutItem::is_droppable_audio) {
                q.items.remove(pos);
            } else {
                q.failed = true;
                q.items.clear();
                cvar.notify_all();
                return false;
            }
        }
        q.items.push_back(item);
        cvar.notify_one();
        true
    }

    fn clear_droppable_audio(&self) {
        let (lock, _) = &*self.queue;
        let mut q = lock.lock().unwrap();
        q.items.retain(|i| !i.is_droppable_audio());
    }

    fn close(&self) {
        let (lock, cvar) = &*self.queue;
        {
            let mut q = lock.lock().unwrap();
            q.closed = true;
            q.items.clear();
        }
        cvar.notify_all();
    }
}

struct ClientOut {
    epoch: u64,
    ssh: SshWriter,
    ssh_open: bool,
    ssh_closer: Box<dyn Fn() + Send>,
    link: Option<Arc<UdpLink>>,
    ssh_client: Option<String>,
    test_hooks: bool,
    voice_opus: bool,
}

impl ClientOut {
    fn udp_alive(&self) -> bool {
        self.link.as_ref().is_some_and(|l| l.alive())
    }

    fn udp_keeping_alive(&self) -> bool {
        self.link.as_ref().is_some_and(|l| {
            l.enabled.load(Ordering::Relaxed) && l.ever_received() && l.silent_us() <= UDP_KEEPALIVE_US
        })
    }

    fn is_connected(&self) -> bool {
        self.ssh_open || self.udp_keeping_alive()
    }

    fn emit_ssh(&mut self, t: u8, payload: &[u8]) -> bool {
        if !self.ssh_open {
            return false;
        }
        if !self.ssh.enqueue(SshOutItem::Frame { ty: t, payload: payload.to_vec() }) {
            self.ssh_open = false;
            self.close_ssh();
            return false;
        }
        true
    }

    fn emit(&mut self, t: u8, payload: &[u8]) -> bool {
        if let Some(l) = self.link.as_ref().filter(|l| l.alive()) {
            l.send_frame(t, payload);
            return true;
        }
        if self.ssh_open {
            return self.emit_ssh(t, payload);
        }
        if self.udp_keeping_alive() {
            if let Some(l) = self.link.as_ref() {
                l.send_frame(t, payload);
                return true;
            }
        }
        false
    }

    fn close_ssh(&mut self) {
        self.ssh_open = false;
        self.ssh.close();
        (self.ssh_closer)();
    }

    fn close(mut self) {
        self.close_ssh();
        if let Some(l) = self.link.take() {
            l.close();
        }
    }
}

struct EngineExit {
    active_client: Option<ClientOut>,
    evict_reply: Option<(mpsc::Sender<bool>, mpsc::Receiver<()>)>,
}

type UdpBinder = Box<dyn Fn(f64) -> Option<Arc<UdpLink>> + Send>;

struct VoiceEngine {
    mic_port: u16,
    session: Session,
    attached_once: bool,
    idle_timeout: Duration,
    bind_udp: UdpBinder,
    ev_tx: mpsc::SyncSender<Event>,
    client_connected: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    client: Option<ClientOut>,
    epoch: u64,
    disconnected_since: Option<Instant>,
    mic_streams: HashMap<u32, MicStreamHandle>,
    spk_streams: HashMap<u32, SpkStreamState>,
    evict_reply: Option<(mpsc::Sender<bool>, mpsc::Receiver<()>)>,
    last_stats_sent: Instant,
}

impl VoiceEngine {
    fn new(
        mic_port: u16,
        session: Session,
        idle_timeout: Duration,
        bind_udp: UdpBinder,
        ev_tx: mpsc::SyncSender<Event>,
        client_connected: Arc<AtomicBool>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            mic_port,
            session,
            attached_once: false,
            idle_timeout,
            bind_udp,
            ev_tx,
            client_connected,
            shutdown,
            client: None,
            epoch: 0,
            disconnected_since: Some(Instant::now()),
            mic_streams: HashMap::new(),
            spk_streams: HashMap::new(),
            evict_reply: None,
            last_stats_sent: Instant::now(),
        }
    }

    fn is_client_reachable(&self) -> bool {
        self.client.as_ref().is_some_and(|c| c.ssh_open || c.udp_alive())
    }

    fn announce_unannounced_streams(&mut self) {
        let Some(c) = self.client.as_mut() else {
            return;
        };
        let epoch = c.epoch;
        let mut mic_ids: Vec<u32> = self
            .mic_streams
            .iter()
            .filter_map(|(&id, m)| (m.announced_to_epoch != Some(epoch)).then_some(id))
            .collect();
        mic_ids.sort_unstable();
        for id in mic_ids {
            if let Some(m) = self.mic_streams.get_mut(&id) {
                if c.emit(ty::MIC_START, &proto::mic_start_payload(id, MIC_SAMPLE_RATE, CHANNELS)) {
                    m.announced_to_epoch = Some(epoch);
                }
            }
        }
        let mut spk_ids: Vec<u32> = self
            .spk_streams
            .iter()
            .filter_map(|(&id, s)| (s.announced_to_epoch != Some(epoch)).then_some(id))
            .collect();
        spk_ids.sort_unstable();
        for id in spk_ids {
            if let Some(s) = self.spk_streams.get_mut(&id) {
                if c.emit(ty::SPK_START, &proto::spk_start_payload(id, s.sample_rate, CHANNELS)) {
                    s.announced_to_epoch = Some(epoch);
                }
            }
        }
    }

    fn next_spk_pace_wait(&self) -> Duration {
        let now = Instant::now();
        let mut wait = Duration::from_millis(15);
        for state in self.spk_streams.values() {
            if let Some(op) = state.opus.as_ref() {
                if !op.tx_queue.is_empty() {
                    let due = op.next_tx_at.unwrap_or(now);
                    let rem = due.saturating_duration_since(now);
                    wait = wait.min(rem.min(Duration::from_millis(5)));
                }
            }
        }
        wait
    }

    fn drain_paced_spk_opus(&mut self) {
        let Some(c) = self.client.as_mut() else { return };
        if !c.voice_opus {
            return;
        }
        let now = Instant::now();
        let pace = Duration::from_millis(SPK_OPUS_PACE_MS);
        let mut to_emit: Vec<Vec<u8>> = Vec::new();
        for state in self.spk_streams.values_mut() {
            let Some(op) = state.opus.as_mut() else { continue };
            let mut sent_this_tick = 0u8;
            while sent_this_tick < 2 {
                let due = op.next_tx_at.unwrap_or(now);
                if op.burst_budget == 0 && now < due {
                    break;
                }
                let Some(pkt) = op.tx_queue.pop_front() else {
                    break;
                };
                to_emit.push(proto::encode_voice_opus(&pkt));
                sent_this_tick += 1;
                if op.burst_budget > 0 {
                    op.burst_budget -= 1;
                    op.next_tx_at = Some(now + pace);
                } else {
                    let next = due + pace;
                    op.next_tx_at = Some(if now > next + Duration::from_millis(60) { now + pace } else { next });
                }
            }
        }
        for payload in to_emit {
            let _ = c.emit(ty::SPK_OPUS, &payload);
        }
    }

    fn run(&mut self, ev_rx: mpsc::Receiver<Event>) -> EngineExit {
        loop {
            match ev_rx.recv_timeout(self.next_spk_pace_wait()) {
                Ok(ev) => {
                    if !self.handle_event(ev) {
                        break;
                    }
                    // Drain a bounded batch of queued events before ticking so bursts don't starve timers.
                    let mut stop = false;
                    let mut drained = 1usize;
                    while drained < MAX_EVENT_DRAIN {
                        match ev_rx.try_recv() {
                            Ok(next) => {
                                drained += 1;
                                if !self.handle_event(next) {
                                    stop = true;
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    if stop {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }

            if let Some(c) = self.client.as_mut() {
                let expired = if let Some(l) = &c.link {
                    l.tick()
                } else {
                    Vec::new()
                };
                let mut unannounced_ids: Vec<(u8, u32)> = Vec::new();
                for (frame_ty, payload) in expired {
                    let sent = if c.ssh_open {
                        c.emit_ssh(frame_ty, &payload)
                    } else {
                        false
                    };
                    if !sent && matches!(frame_ty, ty::MIC_START | ty::SPK_START) && payload.len() >= 4 {
                        unannounced_ids.push((frame_ty, u32::from_le_bytes(payload[0..4].try_into().unwrap())));
                    }
                }
                for (frame_ty, id) in unannounced_ids {
                    match frame_ty {
                        ty::MIC_START => {
                            if let Some(m) = self.mic_streams.get_mut(&id) {
                                m.announced_to_epoch = None;
                            }
                        }
                        ty::SPK_START => {
                            if let Some(s) = self.spk_streams.get_mut(&id) {
                                s.announced_to_epoch = None;
                            }
                        }
                        _ => {}
                    }
                }
                if !c.is_connected() {
                    let old = self.client.take().unwrap();
                    old.close();
                    self.disconnected_since = Some(Instant::now());
                    crate::log!("voice client disconnected (session={})", self.session.session);
                }
            }
            if self.is_client_reachable() {
                self.announce_unannounced_streams();
                self.flush_pending_spk_tails();
                self.drain_paced_spk_opus();
                self.maybe_emit_mic_stats();
            }
            self.client_connected.store(self.is_client_reachable(), Ordering::Relaxed);

            if self.client.is_none() {
                let wait = if self.attached_once {
                    self.idle_timeout
                } else {
                    // Initial grace period for the spawning `voice` CLI to attach over IPC.
                    self.idle_timeout.max(Duration::from_secs(10))
                };
                let since = *self.disconnected_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= wait {
                    break;
                }
            }
        }

        self.shutdown.store(true, Ordering::SeqCst);
        self.client_connected.store(false, Ordering::Relaxed);
        let active_client = self.client.take();
        for (_, m) in self.mic_streams.drain() {
            m.close();
        }
        for (_, s) in self.spk_streams.drain() {
            s.stop_flag.store(true, Ordering::SeqCst);
        }
        EngineExit {
            active_client,
            evict_reply: self.evict_reply.take(),
        }
    }

    fn handle_event(&mut self, ev: Event) -> bool {
        match ev {
            Event::Shutdown => return false,
            Event::ClientAttach { req, input, output, closer, reply_tx } => {
                self.handle_attach(req, input, output, closer, reply_tx);
            }
            Event::ClientFrame { epoch, msg } => {
                if self.client.as_ref().is_some_and(|c| c.epoch == epoch) && !self.handle_client_msg(msg) {
                    return false;
                }
            }
            Event::EvictIdle { port, reply_tx, flushed_rx } => {
                if self.mic_port == port
                    && self.attached_once
                    && !self.client.as_ref().is_some_and(|c| c.is_connected())
                {
                    crate::log!("voice-daemon evicted idle session={} mic_port={port}", self.session.session);
                    self.evict_reply = Some((reply_tx, flushed_rx));
                    return false;
                }
                let _ = reply_tx.send(false);
            }
            Event::ForceShutdown { reply_tx, flushed_rx } => {
                crate::log!("voice-daemon forced shutdown session={}", self.session.session);
                self.evict_reply = Some((reply_tx, flushed_rx));
                return false;
            }
            Event::ClientSshClosed { epoch } => {
                if let Some(c) = self.client.as_mut() {
                    if c.epoch == epoch {
                        c.close_ssh();
                    }
                }
            }
            Event::AgyMicOpen { stream_id, mut handle } => {
                if let Some(c) = self.client.as_mut() {
                    if c.emit(ty::MIC_START, &proto::mic_start_payload(stream_id, MIC_SAMPLE_RATE, CHANNELS)) {
                        handle.announced_to_epoch = Some(c.epoch);
                    }
                }
                self.mic_streams.insert(stream_id, handle);
            }
            Event::AgyMicClosed { stream_id } => {
                if let Some(handle) = self.mic_streams.remove(&stream_id) {
                    if let Some(c) = self.client.as_mut() {
                        if handle.announced_to_epoch == Some(c.epoch) {
                            let _ = c.emit(ty::MIC_STOP, &proto::mic_stop_payload(stream_id));
                        }
                    }
                }
            }
            Event::AgySpkOpen { stream_id, sample_rate, ack_tx, stop_flag } => {
                let link_ctrl = VoiceLinkController::for_sample_rate(sample_rate);
                let opus = if is_opus_sample_rate(sample_rate) {
                    SpkOpusState::new(sample_rate, link_ctrl.profile().bitrate).ok()
                } else {
                    None
                };
                let mut state = SpkStreamState {
                    sample_rate,
                    ack_tx: Some(ack_tx),
                    stop_flag,
                    announced_to_epoch: None,
                    opus,
                    link_ctrl,
                };
                if let Some(c) = self.client.as_mut() {
                    if c.emit(ty::SPK_START, &proto::spk_start_payload(stream_id, sample_rate, CHANNELS)) {
                        state.announced_to_epoch = Some(c.epoch);
                    }
                }
                self.spk_streams.insert(stream_id, state);
            }
            Event::AgySpkAudio { stream_id, pcm } => {
                if let Some(state) = self.spk_streams.get_mut(&stream_id) {
                    if let Some(c) = self.client.as_mut() {
                        if c.voice_opus {
                            if let Some(op) = state.opus.as_mut() {
                                let profile = state.link_ctrl.profile();
                                op.encoder.apply_params(profile.bitrate, profile.opus_loss_perc);
                                s16le_bytes_to_i16(&pcm, &mut op.scratch);
                                op.pcm_pending.extend_from_slice(&op.scratch);
                                let flen = op.encoder.frame_len();
                                let now = Instant::now();
                                while op.pcm_pending.len() >= flen {
                                    let frame: Vec<i16> = op.pcm_pending.drain(..flen).collect();
                                    if let Some(pkt) = op.encode_frame(stream_id, &frame, profile.red_count) {
                                        op.enqueue_tx(pkt, now);
                                    }
                                }
                                op.last_pcm_at = if op.pcm_pending.is_empty() { None } else { Some(now) };
                                self.drain_paced_spk_opus();
                                return true;
                            }
                        }
                        let _ = c.emit(ty::SPK_AUDIO, &proto::spk_audio_payload(stream_id, &pcm));
                    }
                }
            }
            Event::AgySpkClear { stream_id } => {
                if let Some(state) = self.spk_streams.get_mut(&stream_id) {
                    if let Some(op) = state.opus.as_mut() {
                        op.clear();
                    }
                    if let Some(c) = self.client.as_mut() {
                        c.ssh.clear_droppable_audio();
                        let _ = c.emit(ty::SPK_CLEAR, &proto::spk_clear_payload(stream_id));
                    }
                }
            }
            Event::AgySpkClosed { stream_id, notify_client } => {
                if let Some(mut state) = self.spk_streams.remove(&stream_id) {
                    if notify_client {
                        if let Some(c) = self.client.as_mut() {
                            if state.announced_to_epoch == Some(c.epoch) {
                                if c.voice_opus {
                                    if let Some(op) = state.opus.as_mut() {
                                        if !op.pcm_pending.is_empty() {
                                            let flen = op.encoder.frame_len();
                                            let mut frame = std::mem::take(&mut op.pcm_pending);
                                            frame.resize(flen, 0);
                                            op.last_pcm_at = None;
                                            let profile = state.link_ctrl.profile();
                                            op.encoder.apply_params(profile.bitrate, profile.opus_loss_perc);
                                            if let Some(pkt) = op.encode_frame(stream_id, &frame, profile.red_count) {
                                                op.enqueue_tx(pkt, Instant::now());
                                            }
                                        }
                                        while let Some(pkt) = op.tx_queue.pop_front() {
                                            let _ = c.emit(ty::SPK_OPUS, &proto::encode_voice_opus(&pkt));
                                        }
                                    }
                                }
                                let _ = c.emit(ty::SPK_STOP, &proto::spk_stop_payload(stream_id));
                            }
                        }
                    }
                }
            }
        }
        true
    }

    fn handle_attach(
        &mut self,
        req: AttachReq,
        input: Box<dyn Read + Send>,
        output: Box<dyn Write + Send>,
        closer: Box<dyn Fn() + Send>,
        reply_tx: mpsc::Sender<bool>,
    ) {
        if req.proto != proto::PROTO {
            let _ = reply_tx.send(false);
            return;
        }
        // After the very first attach, any subsequent attach to this per-session daemon MUST
        // present the matching session + resume_token (unauthorized tokens never kick the owner).
        if self.attached_once {
            let authorized = matches!(
                (&req.resume, &req.token),
                (Some(r), Some(t)) if r == &self.session.session && session::same_secret(&self.session.resume_token, t)
            );
            if !authorized {
                let _ = reply_tx.send(false);
                return;
            }
        }

        if let Some(old) = self.client.take() {
            // Valid resume silently replaces the prior controller; stamp disconnected_since
            // immediately so any early return below never leaks the daemon without an idle timer.
            self.disconnected_since = Some(Instant::now());
            self.client_connected.store(false, Ordering::Relaxed);
            old.close();
        } else if self.disconnected_since.is_none() {
            self.disconnected_since = Some(Instant::now());
        }
        if let Some(secs) = req.idle_timeout {
            self.idle_timeout = Duration::from_secs(secs);
        }

        // Confirm attach to the IPC handler BEFORE queueing `VoiceHello` so `voice::cmd` sees `OK\n` first.
        if reply_tx.send(true).is_err() {
            closer();
            return;
        }

        let link = (self.bind_udp)(1_000_000.0);
        let hello = VoiceHello {
            proto: proto::PROTO,
            ver: env!("CARGO_PKG_VERSION").into(),
            os: crate::OS.into(),
            mode: "voice".into(),
            mic_port: self.mic_port,
            session: self.session.session.clone(),
            resume_token: self.session.resume_token.clone(),
            udp: link.as_ref().map(|l| proto::Udp { port: l.port, key: session::b64(&l.key) }),
            codecs: vec!["opus".into(), "pcm16".into()],
        };
        let mut hello_line = serde_json::to_string(&hello).unwrap();
        hello_line.push('\n');

        self.attached_once = true;
        self.disconnected_since = None;
        self.epoch += 1;
        let epoch = self.epoch;

        let ssh = SshWriter::spawn(output, epoch, self.ev_tx.clone());
        let ssh_open = ssh.enqueue(SshOutItem::Raw(hello_line.into_bytes()));

        if let Some(l) = &link {
            let (utx, urx) = mpsc::channel::<Option<ClientMsg>>();
            l.start(utx);
            let ev_tx = self.ev_tx.clone();
            std::thread::spawn(move || {
                while let Ok(Some(msg)) = urx.recv() {
                    if ev_tx.send(Event::ClientFrame { epoch, msg }).is_err() {
                        break;
                    }
                }
            });
        }

        let ev_tx = self.ev_tx.clone();
        std::thread::spawn(move || {
            let mut r = BufReader::new(input);
            loop {
                match proto::read_frame(&mut r) {
                    Ok(Some((t, p))) => match proto::decode_client(t, &p) {
                        Ok(msg) => {
                            if ev_tx.send(Event::ClientFrame { epoch, msg }).is_err() {
                                return;
                            }
                        }
                        Err(e) => crate::log!("voice bad frame 0x{t:02x}: {e}"),
                    },
                    _ => {
                        let _ = ev_tx.send(Event::ClientSshClosed { epoch });
                        return;
                    }
                }
            }
        });

        let out = ClientOut {
            epoch,
            ssh,
            ssh_open,
            ssh_closer: closer,
            link,
            ssh_client: req.ssh_client,
            test_hooks: std::env::var("ROAMTTY_DESKTOP_TEST").as_deref() == Ok("1"),
            voice_opus: false,
        };

        self.client = Some(out);
        self.announce_unannounced_streams();
        self.client_connected.store(self.is_client_reachable(), Ordering::Relaxed);
    }

    fn flush_pending_spk_tails(&mut self) {
        let Some(c) = self.client.as_mut() else { return };
        if !c.voice_opus {
            return;
        }
        let now = Instant::now();
        for (&stream_id, state) in &mut self.spk_streams {
            let Some(op) = state.opus.as_mut() else { continue };
            if op.pcm_pending.is_empty() {
                continue;
            }
            if op.last_pcm_at.is_some_and(|t| now.duration_since(t) >= Duration::from_millis(SPK_TAIL_FLUSH_MS)) {
                let flen = op.encoder.frame_len();
                let mut frame = std::mem::take(&mut op.pcm_pending);
                frame.resize(flen, 0);
                op.last_pcm_at = None;
                let profile = state.link_ctrl.profile();
                op.encoder.apply_params(profile.bitrate, profile.opus_loss_perc);
                if let Some(pkt) = op.encode_frame(stream_id, &frame, profile.red_count) {
                    op.enqueue_tx(pkt, now);
                }
            }
        }
    }

    fn maybe_emit_mic_stats(&mut self) {
        if self.last_stats_sent.elapsed() < Duration::from_millis(VOICE_STATS_INTERVAL_MS) {
            return;
        }
        let Some(c) = self.client.as_mut() else { return };
        if !c.voice_opus {
            return;
        }
        self.last_stats_sent = Instant::now();
        let now_ms = crate::serve::now_us() / 1000;
        let mut to_send = Vec::new();
        for m in self.mic_streams.values() {
            if let Some(st) = m.stats_snapshot(now_ms) {
                to_send.push(st);
            }
        }
        for st in to_send {
            let _ = c.emit(ty::VOICE_STATS_S2C, &proto::encode_voice_stats(&st));
        }
    }

    fn handle_client_msg(&mut self, msg: ClientMsg) -> bool {
        match msg {
            ClientMsg::Config(c) => {
                if c.shutdown == Some(true) {
                    crate::log!("voice client requested shutdown (session={})", self.session.session);
                    return false;
                }
                if let Some(out) = self.client.as_mut() {
                    if let Some(codec) = c.voice_codec.as_deref() {
                        let enable_opus = codec.eq_ignore_ascii_case("opus");
                        if out.voice_opus != enable_opus {
                            out.voice_opus = enable_opus;
                            crate::log!("voice codec negotiated: {}", if enable_opus { "opus" } else { "pcm16" });
                        }
                    }
                    if let (Some(on), Some(l)) = (c.udp, &out.link) {
                        l.enabled.store(on, Ordering::Relaxed);
                    }
                    if let (Some(port), Some(l)) = (c.udp_client_port, &out.link) {
                        let ip = out
                            .ssh_client
                            .as_deref()
                            .and_then(|v| v.split_ascii_whitespace().next())
                            .and_then(|ip| ip.parse::<IpAddr>().ok());
                        if let Some(ip) = ip {
                            l.punch(SocketAddr::new(ip, port));
                        }
                    }
                    if let (Some(loss), Some(l), true) = (c.debug_loss, &out.link, out.test_hooks) {
                        l.drop_ppm.store((loss.clamp(0.0, 1.0) * 1_000_000.0) as u32, Ordering::Relaxed);
                        crate::log!("voice test hook: udp loss {loss}");
                    }
                }
            }
            ClientMsg::Ping(t1) => {
                if let Some(out) = self.client.as_mut() {
                    let t2 = crate::serve::now_us();
                    let _ = out.emit(ty::PONG, &proto::pong_payload(t1, t2, crate::serve::now_us()));
                }
            }
            ClientMsg::MicAudio { stream_id, pcm } => {
                if pcm.is_empty() {
                    return true;
                }
                if pcm.len() % 2 != 0 {
                    crate::log!("voice dropping odd-length MIC_AUDIO ({} bytes) for stream {stream_id}", pcm.len());
                    return true;
                }
                if let Some(m) = self.mic_streams.get(&stream_id) {
                    m.push(pcm);
                }
            }
            ClientMsg::MicOpus(pkt) => {
                if let Some(m) = self.mic_streams.get(&pkt.stream_id) {
                    let now_ms = crate::serve::now_us() / 1000;
                    m.push_opus(pkt.stream_id, &pkt, now_ms);
                }
            }
            ClientMsg::VoiceStats(st) => {
                let now_ms = crate::serve::now_us() / 1000;
                if let Some(state) = self.spk_streams.get_mut(&st.stream_id) {
                    let profile = state.link_ctrl.on_stats(&st, now_ms);
                    if let Some(op) = state.opus.as_mut() {
                        op.encoder.apply_params(profile.bitrate, profile.opus_loss_perc);
                    }
                }
            }
            ClientMsg::SpkAck { stream_id, ok, error } => {
                if let Some(state) = self.spk_streams.get_mut(&stream_id) {
                    if let Some(ack_tx) = state.ack_tx.take() {
                        let _ = ack_tx.send((ok, error));
                        if !ok {
                            self.spk_streams.remove(&stream_id);
                        }
                    } else if !ok {
                        // Resumed client rejected an already-open speaker stream: close it cleanly.
                        state.stop_flag.store(true, Ordering::SeqCst);
                        self.spk_streams.remove(&stream_id);
                    }
                    // Duplicate SPK_ACK(ok=1) after --resume: safely ignored, never re-sends OK to agy.
                }
            }
            _ => {}
        }
        true
    }
}

// ============================================================================
// `agy` loopback TCP accept & connection handlers
// ============================================================================

fn next_id(counter: &AtomicU32) -> u32 {
    loop {
        let id = counter.fetch_add(1, Ordering::Relaxed);
        if id != 0 {
            return id;
        }
    }
}

struct ConnGuard {
    active: Arc<AtomicUsize>,
}

impl ConnGuard {
    fn try_acquire(active: &Arc<AtomicUsize>) -> Option<Self> {
        let mut cur = active.load(Ordering::Relaxed);
        loop {
            if cur >= MAX_AGY_CONNS {
                return None;
            }
            match active.compare_exchange_weak(cur, cur + 1, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => return Some(Self { active: active.clone() }),
                Err(next) => cur = next,
            }
        }
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

fn spawn_mic_accept_loop(
    listener: TcpListener,
    next_stream_id: Arc<AtomicU32>,
    ev_tx: mpsc::SyncSender<Event>,
    client_connected: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    let _ = listener.set_nonblocking(true);
    let active_conns = Arc::new(AtomicUsize::new(0));
    std::thread::spawn(move || {
        while !shutdown.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let Some(guard) = ConnGuard::try_acquire(&active_conns) else {
                        crate::log!("voice max concurrent agy connections ({MAX_AGY_CONNS}) reached; rejecting");
                        let _ = stream.shutdown(Shutdown::Both);
                        continue;
                    };
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_nodelay(true);
                    let next_stream_id = next_stream_id.clone();
                    let ev_tx = ev_tx.clone();
                    let client_connected = client_connected.clone();
                    let shutdown = shutdown.clone();
                    std::thread::spawn(move || {
                        let _guard = guard;
                        handle_agy_conn(stream, &next_stream_id, ev_tx, client_connected, shutdown);
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::Interrupted
                            | io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::ConnectionReset
                    ) =>
                {
                    continue;
                }
                Err(e) => {
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                    crate::log!("voice mic accept error: {e}");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    })
}

fn write_spk_err_and_close(stream: &mut TcpStream, reason: &str, shutdown: &AtomicBool) {
    let _ = stream.set_write_timeout(Some(MIC_WRITE_TIMEOUT));
    let _ = stream.write_all(&format_spk_err(reason));
    let _ = stream.flush();
    // Half-close write and drain any unread bytes (such as peeked invalid headers) before drop
    // so peers (especially Winsock) receive the ERR line followed by clean FIN instead of RST.
    let _ = stream.shutdown(Shutdown::Write);
    let deadline = Instant::now() + Duration::from_millis(100);
    let mut drain = [0u8; 512];
    let mut drained = 0usize;
    const MAX_DRAIN_BYTES: usize = 64 * 1024;
    while drained < MAX_DRAIN_BYTES && !shutdown.load(Ordering::Relaxed) {
        let Some(rem) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else {
            break;
        };
        let _ = stream.set_read_timeout(Some(rem.min(Duration::from_millis(50))));
        match stream.read(&mut drain) {
            Ok(0) => break,
            Ok(n) => drained += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

fn handle_agy_conn(
    mut stream: TcpStream,
    next_stream_id: &AtomicU32,
    ev_tx: mpsc::SyncSender<Event>,
    client_connected: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) {
    match classify_connection(&stream) {
        ConnKind::Eof => {
            let _ = stream.shutdown(Shutdown::Both);
        }
        ConnKind::Invalid => {
            write_spk_err_and_close(&mut stream, "invalid header", &shutdown);
        }
        ConnKind::Speaker => handle_speaker_conn(stream, next_stream_id, ev_tx, client_connected, shutdown),
        ConnKind::Mic => handle_mic_conn(stream, next_stream_id, ev_tx, shutdown),
    }
}

fn handle_speaker_conn(
    stream: TcpStream,
    next_stream_id: &AtomicU32,
    ev_tx: mpsc::SyncSender<Event>,
    client_connected: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) {
    let Ok(mut write_half) = stream.try_clone() else {
        return;
    };
    let _ = write_half.set_write_timeout(Some(MIC_WRITE_TIMEOUT));
    let mut reader = BufReader::with_capacity(8192, stream);

    let sample_rate = match read_speaker_header(&mut reader, &shutdown) {
        Ok(rate) => rate,
        Err(reason) => {
            write_spk_err_and_close(&mut write_half, reason, &shutdown);
            return;
        }
    };

    let stream_id = next_id(next_stream_id);
    let (ack_tx, ack_rx) = mpsc::channel::<(bool, String)>();
    let stop_flag = Arc::new(AtomicBool::new(false));

    if ev_tx
        .send(Event::AgySpkOpen {
            stream_id,
            sample_rate,
            ack_tx,
            stop_flag: stop_flag.clone(),
        })
        .is_err()
    {
        write_spk_err_and_close(&mut write_half, "bridge unavailable", &shutdown);
        return;
    }

    match ack_rx.recv_timeout(SPK_ACK_TIMEOUT) {
        Ok((true, _)) => {
            if write_half.write_all(format_spk_ok()).and_then(|_| write_half.flush()).is_err() {
                let _ = ev_tx.send(Event::AgySpkClosed { stream_id, notify_client: true });
                let _ = write_half.shutdown(Shutdown::Both);
                return;
            }
        }
        Ok((false, err)) => {
            write_spk_err_and_close(&mut write_half, &err, &shutdown);
            let _ = ev_tx.send(Event::AgySpkClosed { stream_id, notify_client: false });
            return;
        }
        Err(_) => {
            let reason = if client_connected.load(Ordering::Relaxed) {
                "speaker ack timeout"
            } else {
                "no voice client connected"
            };
            write_spk_err_and_close(&mut write_half, reason, &shutdown);
            let _ = ev_tx.send(Event::AgySpkClosed { stream_id, notify_client: true });
            return;
        }
    }

    // Read 5-byte JETSKI-AUDIO/1 frames: [type: u8][len: u32 LE] + payload (max 1 MiB).
    let mut head = [0u8; 5];
    'frames: loop {
        match read_exact_interruptible(&mut reader, &mut head, &stop_flag, &shutdown, None) {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => {
                crate::log!("voice speaker {stream_id} header read error: {e}");
                break;
            }
        }
        let frame_type = head[0];
        let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
        if len > SPK_FRAME_MAX_LEN {
            crate::log!("voice speaker {stream_id} frame too large: {len} bytes");
            break;
        }
        match frame_type {
            b'A' => {
                if len == 0 {
                    continue;
                }
                if !len.is_multiple_of(2) {
                    crate::log!("voice speaker {stream_id} odd-length 'A' frame len={len}");
                    break;
                }
                // Stream payload incrementally in MAX_SPK_AUDIO_CHUNK (960B) buffers rather than
                // allocating up to 1 MiB upfront.
                let mut remaining = len;
                let deadline = Instant::now() + SPK_FRAME_READ_TIMEOUT;
                while remaining > 0 {
                    let take = remaining.min(MAX_SPK_AUDIO_CHUNK);
                    let mut chunk = vec![0u8; take];
                    match read_exact_interruptible(&mut reader, &mut chunk, &stop_flag, &shutdown, Some(deadline)) {
                        Ok(true) => {
                            remaining -= take;
                            if ev_tx.send(Event::AgySpkAudio { stream_id, pcm: chunk }).is_err() {
                                break 'frames;
                            }
                        }
                        Ok(false) => break 'frames,
                        Err(e) => {
                            crate::log!("voice speaker {stream_id} payload read error: {e}");
                            break 'frames;
                        }
                    }
                }
            }
            b'C' => {
                if len != 0 {
                    crate::log!("voice speaker {stream_id} invalid 'C' frame len={len}");
                    break;
                }
                if ev_tx.send(Event::AgySpkClear { stream_id }).is_err() {
                    break;
                }
            }
            other => {
                crate::log!("voice speaker {stream_id} unknown frame type 0x{other:02x}");
                break;
            }
        }
    }

    let _ = ev_tx.send(Event::AgySpkClosed { stream_id, notify_client: true });
    let _ = write_half.shutdown(Shutdown::Both);
}

fn handle_mic_conn(
    stream: TcpStream,
    next_stream_id: &AtomicU32,
    ev_tx: mpsc::SyncSender<Event>,
    shutdown: Arc<AtomicBool>,
) {
    let Ok(mut write_stream) = stream.try_clone() else {
        return;
    };
    let Ok(close_stream) = stream.try_clone() else {
        return;
    };
    let mut read_stream = stream;
    let _ = write_stream.set_write_timeout(Some(MIC_WRITE_TIMEOUT));
    let _ = read_stream.set_read_timeout(Some(Duration::from_millis(150)));

    let stream_id = next_id(next_stream_id);
    let queue = Arc::new((
        Mutex::new(MicQueue { chunks: VecDeque::new(), closed: false }),
        Condvar::new(),
    ));
    let opus_jb: Arc<Mutex<Option<VoiceJitterBuffer>>> = Arc::new(Mutex::new(None));
    let notified = Arc::new(AtomicBool::new(false));
    let handle = MicStreamHandle {
        queue: queue.clone(),
        opus_jb: opus_jb.clone(),
        sock: close_stream,
        announced_to_epoch: None,
    };

    if ev_tx.send(Event::AgyMicOpen { stream_id, handle }).is_err() {
        let _ = read_stream.shutdown(Shutdown::Both);
        return;
    }

    // Writer thread: forwards legacy MIC_AUDIO chunks immediately, OR drives real-time 20 ms
    // Opus jitter-buffer playout (with RED + LBRR + PLC + DTX silence continuity) to `agy`.
    let w_queue = queue.clone();
    let w_opus_jb = opus_jb.clone();
    let w_notified = notified.clone();
    let w_ev_tx = ev_tx.clone();
    let writer_thread = std::thread::spawn(move || {
        let (lock, cvar) = &*w_queue;
        let mut had_error = false;
        let mut next_opus_tick: Option<Instant> = None;
        let mut ever_played_opus = false;
        let silence_20ms = vec![0u8; frame_samples(MIC_SAMPLE_RATE) * 2];
        loop {
            let opus_active = w_opus_jb.lock().unwrap().is_some();
            if opus_active {
                let target_tick = *next_opus_tick.get_or_insert_with(Instant::now);
                loop {
                    let now = Instant::now();
                    if now >= target_tick {
                        break;
                    }
                    let wait = target_tick.saturating_duration_since(now);
                    let q = lock.lock().unwrap();
                    if q.closed || !q.chunks.is_empty() {
                        break;
                    }
                    let _ = cvar.wait_timeout(q, wait).unwrap();
                }
                // Drain any legacy raw chunks if present.
                let legacy = {
                    let mut q = lock.lock().unwrap();
                    (q.chunks.pop_front(), q.closed)
                };
                if let Some(pcm) = legacy.0 {
                    if let Err(e) = write_stream.write_all(&pcm) {
                        crate::log!("voice mic {stream_id} write error: {e}");
                        had_error = true;
                        let mut q = lock.lock().unwrap();
                        q.closed = true;
                        q.chunks.clear();
                        cvar.notify_all();
                        break;
                    }
                    continue;
                }
                let now_ms = crate::serve::now_us() / 1000;
                if legacy.1 {
                    // Drain all remaining buffered Opus frames before sending TCP FIN to `agy`.
                    // Advance `now_ms` by 200 ms so any sub-pre-roll tail still flushes cleanly.
                    let drain_now = now_ms.saturating_add(200);
                    for _ in 0..64 {
                        let popped = {
                            let mut jb_guard = w_opus_jb.lock().unwrap();
                            jb_guard.as_mut().and_then(|jb| {
                                if jb.queued_frames() > 0 {
                                    jb.pop_frame(drain_now)
                                } else {
                                    None
                                }
                            })
                        };
                        let Some(samples) = popped else {
                            break;
                        };
                        let pcm = i16_to_s16le_bytes(&samples);
                        if let Err(e) = write_stream.write_all(&pcm) {
                            crate::log!("voice mic {stream_id} opus drain write error: {e}");
                            had_error = true;
                            break;
                        }
                    }
                    break;
                }
                let popped = {
                    let mut jb_guard = w_opus_jb.lock().unwrap();
                    jb_guard.as_mut().and_then(|jb| jb.pop_frame(now_ms))
                };
                let mut played_samples: Option<usize> = None;
                if let Some(samples) = popped {
                    ever_played_opus = true;
                    played_samples = Some(samples.len());
                    let pcm = i16_to_s16le_bytes(&samples);
                    if let Err(e) = write_stream.write_all(&pcm) {
                        crate::log!("voice mic {stream_id} opus write error: {e}");
                        had_error = true;
                        let mut q = lock.lock().unwrap();
                        q.closed = true;
                        q.chunks.clear();
                        cvar.notify_all();
                        break;
                    }
                } else if ever_played_opus {
                    if let Err(e) = write_stream.write_all(&silence_20ms) {
                        crate::log!("voice mic {stream_id} opus silence write error: {e}");
                        had_error = true;
                        let mut q = lock.lock().unwrap();
                        q.closed = true;
                        q.chunks.clear();
                        cvar.notify_all();
                        break;
                    }
                }
                // Advance playout clock by actual played sample duration so WSOLA-compressed frames drain backlog.
                let step = match played_samples {
                    Some(n) if n > 0 => {
                        let us = (n as u64).saturating_mul(1_000_000) / (MIC_SAMPLE_RATE as u64);
                        Duration::from_micros(us.clamp(10_000, 25_000))
                    }
                    _ => Duration::from_millis(crate::voice_codec::FRAME_MS as u64),
                };
                let after = Instant::now();
                let next = target_tick + step;
                next_opus_tick = Some(if after > next + Duration::from_millis(60) { after + step } else { next });
                continue;
            }

            let chunk = {
                let mut q = lock.lock().unwrap();
                while q.chunks.is_empty() && !q.closed && w_opus_jb.lock().unwrap().is_none() {
                    q = cvar.wait(q).unwrap();
                }
                if let Some(pcm) = q.chunks.pop_front() {
                    Some(pcm)
                } else if q.closed {
                    break;
                } else {
                    None
                }
            };
            let Some(pcm) = chunk else {
                continue;
            };
            if let Err(e) = write_stream.write_all(&pcm) {
                crate::log!("voice mic {stream_id} write error: {e}");
                had_error = true;
                let mut q = lock.lock().unwrap();
                q.closed = true;
                q.chunks.clear();
                cvar.notify_all();
                break;
            }
        }
        let _ = write_stream.flush();
        if had_error {
            let _ = write_stream.shutdown(Shutdown::Both);
        } else {
            // Graceful FIN after all written PCM bytes so `agy`'s 2s drain receives clean EOF.
            let _ = write_stream.shutdown(Shutdown::Write);
        }
        if !w_notified.swap(true, Ordering::SeqCst) {
            let _ = w_ev_tx.send(Event::AgyMicClosed { stream_id });
        }
    });

    // Reader loop: detects `agy` half-close (FIN / EOF) or reset.
    let mut buf = [0u8; 64];
    let mut logged_unexpected = false;
    let (lock, cvar) = &*queue;
    loop {
        if shutdown.load(Ordering::Relaxed) || lock.lock().unwrap().closed {
            break;
        }
        match read_stream.read(&mut buf) {
            Ok(0) => break, // `agy` half-closed write end (FIN) to stop recording.
            Ok(n) => {
                if !logged_unexpected {
                    crate::log!("voice mic {stream_id} received unexpected {n} bytes from peer; ignoring");
                    logged_unexpected = true;
                }
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted) => {
                continue;
            }
            Err(_) => break,
        }
    }

    {
        let mut q = lock.lock().unwrap();
        q.closed = true;
    }
    cvar.notify_all();
    let _ = writer_thread.join();
    let _ = read_stream.shutdown(Shutdown::Both);
    if !notified.swap(true, Ordering::SeqCst) {
        let _ = ev_tx.send(Event::AgyMicClosed { stream_id });
    }
}

// ============================================================================
// Per-session IPC transport (Unix domain socket on Unix, loopback TCP on Windows)
// ============================================================================

trait IpcConn: Read + Write + Send {
    fn split(&self) -> io::Result<Box<dyn IpcConn>>;
    fn set_io_timeout(&self, dur: Option<Duration>);
    fn set_write_timeout(&self, dur: Option<Duration>);
    fn shutdown_write(&self);
    fn close(&self);
}

impl IpcConn for TcpStream {
    fn split(&self) -> io::Result<Box<dyn IpcConn>> {
        Ok(Box::new(self.try_clone()?))
    }
    fn set_io_timeout(&self, dur: Option<Duration>) {
        let _ = self.set_read_timeout(dur);
        let _ = self.set_write_timeout(dur);
    }
    fn set_write_timeout(&self, dur: Option<Duration>) {
        let _ = TcpStream::set_write_timeout(self, dur);
    }
    fn shutdown_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
    fn close(&self) {
        let _ = self.shutdown(Shutdown::Both);
    }
}

fn spawn_ipc_accept_loop(
    listener: Box<dyn Iterator<Item = Box<dyn IpcConn>> + Send>,
    ipc_token: String,
    ev_tx: mpsc::SyncSender<Event>,
    shutdown: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        for mut conn in listener {
            if shutdown.load(Ordering::Relaxed) {
                conn.close();
                break;
            }
            let ipc_token = ipc_token.clone();
            let ev_tx = ev_tx.clone();
            std::thread::spawn(move || {
                conn.set_io_timeout(Some(IPC_HANDSHAKE_TIMEOUT));
                let Ok(mut read_half) = conn.split() else {
                    conn.close();
                    return;
                };
                let Ok(closer_half) = conn.split() else {
                    conn.close();
                    return;
                };
                let line = match read_line_unbuffered(&mut *read_half, 4096) {
                    Ok(l) => l,
                    Err(_) => {
                        conn.close();
                        return;
                    }
                };
                let Ok(req) = serde_json::from_str::<AttachReq>(&line) else {
                    let _ = writeln!(conn, "ERR bad request");
                    conn.close();
                    return;
                };
                if !session::same_secret(&req.ipc_token, &ipc_token) {
                    let _ = writeln!(conn, "ERR bad ipc token");
                    conn.close();
                    return;
                }
                let Ok(mut status_writer) = conn.split() else {
                    conn.close();
                    return;
                };
                if req.shutdown {
                    let (reply_tx, reply_rx) = mpsc::channel::<bool>();
                    let (flushed_tx, flushed_rx) = mpsc::channel::<()>();
                    if ev_tx.send(Event::ForceShutdown { reply_tx, flushed_rx }).is_err() {
                        let _ = writeln!(status_writer, "ERR shutting down");
                        let _ = status_writer.flush();
                        status_writer.close();
                        return;
                    }
                    match reply_rx.recv() {
                        Ok(true) => {
                            let _ = writeln!(status_writer, "EVICTED");
                            let _ = status_writer.flush();
                            status_writer.close();
                            let _ = flushed_tx.send(());
                        }
                        _ => {
                            let _ = writeln!(status_writer, "ERR shutting down");
                            let _ = status_writer.flush();
                            status_writer.close();
                        }
                    }
                    return;
                }
                if let Some(port) = req.evict_idle_port {
                    let (reply_tx, reply_rx) = mpsc::channel::<bool>();
                    let (flushed_tx, flushed_rx) = mpsc::channel::<()>();
                    if ev_tx.send(Event::EvictIdle { port, reply_tx, flushed_rx }).is_err() {
                        let _ = writeln!(status_writer, "ERR shutting down");
                        let _ = status_writer.flush();
                        status_writer.close();
                        return;
                    }
                    match reply_rx.recv() {
                        Ok(true) => {
                            let _ = writeln!(status_writer, "EVICTED");
                            let _ = status_writer.flush();
                            status_writer.close();
                            let _ = flushed_tx.send(());
                        }
                        Ok(false) => {
                            let _ = writeln!(status_writer, "BUSY");
                            let _ = status_writer.flush();
                            status_writer.close();
                        }
                        Err(_) => {
                            let _ = writeln!(status_writer, "ERR shutting down");
                            let _ = status_writer.flush();
                            status_writer.close();
                        }
                    }
                    return;
                }
                // Clear read timeout before handing `read_half` to the long-lived SSH reader thread,
                // while keeping a short write timeout so a stalled `relay()` cannot wedge `SshWriter`.
                conn.set_io_timeout(None);
                conn.set_write_timeout(Some(SSH_WRITE_TIMEOUT));

                let (reply_tx, reply_rx) = mpsc::channel::<bool>();
                // `OkPrefixWriter` emits `OK\n` on first write so `OK\n` is guaranteed to precede
                // `VoiceHello` on the wire without racing across threads.
                let prefixed_out = Box::new(OkPrefixWriter { inner: conn, sent_ok: false });
                let closer: Box<dyn Fn() + Send> = Box::new(move || closer_half.close());
                if ev_tx
                    .send(Event::ClientAttach {
                        req,
                        input: Box::new(read_half),
                        output: prefixed_out,
                        closer,
                        reply_tx,
                    })
                    .is_err()
                {
                    let _ = writeln!(status_writer, "ERR shutting down");
                    status_writer.close();
                    return;
                }
                match reply_rx.recv() {
                    Ok(true) => {} // `OkPrefixWriter` emits `OK\n` immediately before `VoiceHello`.
                    _ => {
                        let _ = writeln!(status_writer, "DENIED");
                        let _ = status_writer.flush();
                        status_writer.close();
                    }
                }
            });
        }
    });
}

struct OkPrefixWriter {
    inner: Box<dyn IpcConn>,
    sent_ok: bool,
}

impl Write for OkPrefixWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !self.sent_ok {
            self.inner.write_all(b"OK\n")?;
            self.sent_ok = true;
        }
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

mod ipc {
    use super::*;

    fn port_path(sess: &str) -> PathBuf {
        voice_dir().join(format!("{sess}.ipc"))
    }

    fn token_path(sess: &str) -> PathBuf {
        voice_dir().join(format!("{sess}.token"))
    }

    pub fn read_token(sess: &str) -> Option<String> {
        std::fs::read_to_string(token_path(sess)).ok()
    }

    fn connect_tcp(port_file: &Path) -> io::Result<Box<dyn IpcConn>> {
        let port: u16 = std::fs::read_to_string(port_file)?
            .trim()
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad ipc port"))?;
        let s = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(300))?;
        let _ = s.set_nodelay(true);
        Ok(Box::new(s))
    }

    fn listen_tcp(
        port_file: &Path,
        shutdown: Arc<AtomicBool>,
    ) -> io::Result<Box<dyn Iterator<Item = Box<dyn IpcConn>> + Send>> {
        let l = TcpListener::bind(("127.0.0.1", 0))?;
        let port = l.local_addr()?.port();
        paths::write_private(port_file, port.to_string().as_bytes())?;
        Ok(Box::new(std::iter::from_fn(move || loop {
            if shutdown.load(Ordering::Relaxed) {
                return None;
            }
            match l.accept() {
                Ok((s, _)) => {
                    let _ = s.set_nodelay(true);
                    return Some(Box::new(s) as Box<dyn IpcConn>);
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::Interrupted
                            | io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::ConnectionReset
                    ) =>
                {
                    continue;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => {
                    if shutdown.load(Ordering::Relaxed) {
                        return None;
                    }
                    crate::log!("voice ipc tcp accept error: {e}");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        })))
    }

    #[cfg(unix)]
    use std::os::unix::net::{UnixListener, UnixStream};

    // macOS `sockaddr_un.sun_path` is 104 bytes (103 + NUL). If `ROAMTTY_DESKTOP_ROOT` is long
    // (e.g. macOS `$TMPDIR`), fall back transparently to loopback TCP authenticated by the 0600 token.
    #[cfg(unix)]
    const MAX_UNIX_PATH_BYTES: usize = 96;

    #[cfg(unix)]
    fn sock_path(sess: &str) -> PathBuf {
        voice_dir().join(format!("{sess}.sock"))
    }

    #[cfg(unix)]
    impl IpcConn for UnixStream {
        fn split(&self) -> io::Result<Box<dyn IpcConn>> {
            Ok(Box::new(self.try_clone()?))
        }
        fn set_io_timeout(&self, dur: Option<Duration>) {
            let _ = self.set_read_timeout(dur);
            let _ = self.set_write_timeout(dur);
        }
        fn set_write_timeout(&self, dur: Option<Duration>) {
            let _ = UnixStream::set_write_timeout(self, dur);
        }
        fn shutdown_write(&self) {
            let _ = self.shutdown(Shutdown::Write);
        }
        fn close(&self) {
            let _ = self.shutdown(Shutdown::Both);
        }
    }

    #[cfg(unix)]
    pub fn connect(sess: &str) -> io::Result<Box<dyn IpcConn>> {
        let sp = sock_path(sess);
        if sp.exists() {
            return Ok(Box::new(UnixStream::connect(&sp)?));
        }
        connect_tcp(&port_path(sess))
    }

    #[cfg(unix)]
    pub fn listen(
        sess: &str,
        token: &str,
        shutdown: Arc<AtomicBool>,
    ) -> io::Result<Box<dyn Iterator<Item = Box<dyn IpcConn>> + Send>> {
        let sp = sock_path(sess);
        let pp = port_path(sess);
        let _ = std::fs::remove_file(&sp);
        let _ = std::fs::remove_file(&pp);
        // Bind `.sock` / `.ipc` FIRST and publish `.token` LAST so concurrent `voice` startups
        // never observe a token without an active listener and delete it as stale.
        let listener: Box<dyn Iterator<Item = Box<dyn IpcConn>> + Send> =
            if sp.as_os_str().len() <= MAX_UNIX_PATH_BYTES {
                let old = unsafe { libc::umask(0o077) };
                let bound = UnixListener::bind(&sp);
                unsafe { libc::umask(old); }
                match bound {
                    Ok(l) => Box::new(std::iter::from_fn(move || loop {
                        if shutdown.load(Ordering::Relaxed) {
                            return None;
                        }
                        match l.accept() {
                            Ok((s, _)) => return Some(Box::new(s) as Box<dyn IpcConn>),
                            Err(e)
                                if matches!(
                                    e.kind(),
                                    io::ErrorKind::Interrupted
                                        | io::ErrorKind::ConnectionAborted
                                        | io::ErrorKind::ConnectionReset
                                ) =>
                            {
                                continue;
                            }
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(10));
                            }
                            Err(e) => {
                                if shutdown.load(Ordering::Relaxed) {
                                    return None;
                                }
                                crate::log!("voice ipc unix accept error: {e}");
                                std::thread::sleep(Duration::from_millis(50));
                            }
                        }
                    })),
                    Err(_) => listen_tcp(&pp, shutdown)?,
                }
            } else {
                listen_tcp(&pp, shutdown)?
            };
        if let Err(e) = paths::write_private(&token_path(sess), token.as_bytes()) {
            cleanup(sess);
            return Err(e);
        }
        Ok(listener)
    }

    #[cfg(unix)]
    pub fn cleanup(sess: &str) {
        let _ = std::fs::remove_file(sock_path(sess));
        let _ = std::fs::remove_file(port_path(sess));
        let _ = std::fs::remove_file(token_path(sess));
    }

    #[cfg(windows)]
    pub fn connect(sess: &str) -> io::Result<Box<dyn IpcConn>> {
        connect_tcp(&port_path(sess))
    }

    #[cfg(windows)]
    pub fn listen(
        sess: &str,
        token: &str,
        shutdown: Arc<AtomicBool>,
    ) -> io::Result<Box<dyn Iterator<Item = Box<dyn IpcConn>> + Send>> {
        let pp = port_path(sess);
        let _ = std::fs::remove_file(&pp);
        let listener = listen_tcp(&pp, shutdown)?;
        if let Err(e) = paths::write_private(&token_path(sess), token.as_bytes()) {
            cleanup(sess);
            return Err(e);
        }
        Ok(listener)
    }

    #[cfg(windows)]
    pub fn cleanup(sess: &str) {
        let _ = std::fs::remove_file(port_path(sess));
        let _ = std::fs::remove_file(token_path(sess));
    }
}

#[cfg(test)]
#[path = "voice_tests.rs"]
mod tests;
