//! Headless `voice` subcommand: bridges remote `agy /voice` (`JETSKI-AUDIO/1` on loopback TCP)
//! over the existing `roamtty-desktop` binary framing and `UdpLink` (with SSH fallback).
//!
//! Zero GUI, AppKit, ScreenCaptureKit, TCC, X11/Wayland, Win32 desktop, or OpenH264 dependencies.

use crate::paths;
use crate::session::{self, Session};
use crate::udp_link::UdpLink;
use proto::{ty, ClientMsg, StartError, VoiceHello};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
const MIC_QUEUE_MAX_CHUNKS: usize = 100;
const UDP_KEEPALIVE_US: u64 = 30_000_000;
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

fn evict_idle_daemons_on_port(port: u16) {
    for sess in list_voice_sessions() {
        let Some(ipc_token) = ipc::read_token(&sess) else { continue };
        let mut conn = match ipc::connect(&sess) {
            Ok(c) => c,
            Err(e) => {
                // On Windows, connecting to an unused loopback TCP port takes ~1s to return WSAECONNREFUSED,
                // so our 300ms loopback connect_timeout surfaces TimedOut for dead IPC ports.
                if matches!(
                    e.kind(),
                    io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::NotFound
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::InvalidData
                ) {
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
        };
        if writeln!(conn, "{}", serde_json::to_string(&req).unwrap()).is_ok() && conn.flush().is_ok() {
            let _ = read_line_unbuffered(&mut *conn, 64);
        }
        conn.close();
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
    let ipc_listener = match ipc::listen(&sess_id, &ipc_token) {
        Ok(l) => l,
        Err(e) => {
            crate::log!("voice-daemon ipc listen failed: {e}");
            ipc::cleanup(&sess_id);
            return 1;
        }
    };
    crate::log!("voice-daemon start session={sess_id} mic_port={mic_port} idle={idle_secs}s");

    let (ev_tx, ev_rx) = mpsc::channel::<Event>();
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
    let evict_reply = engine.run(ev_rx);

    shutdown.store(true, Ordering::SeqCst);
    let _ = mic_accept.join();
    ipc::cleanup(&sess_id);
    if let Some((reply_tx, flushed_rx)) = evict_reply {
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
        if cleanup_stale && e.kind() == io::ErrorKind::ConnectionRefused {
            ipc::cleanup(sess_id);
        }
        e.to_string()
    })?;
    let req = AttachReq {
        ipc_token: ipc_token.trim().to_string(),
        proto: proto_ver,
        resume,
        token,
        idle_timeout,
        ssh_client,
        evict_idle_port: None,
    };
    writeln!(conn, "{}", serde_json::to_string(&req).unwrap()).map_err(|e| e.to_string())?;
    conn.flush().map_err(|e| e.to_string())?;
    let status = read_line_unbuffered(&mut *conn, 64).map_err(|e| e.to_string())?;
    if status.trim() == "OK" {
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
    let (done, ended) = mpsc::channel();
    let up = done.clone();
    std::thread::spawn(move || {
        let _ = io::copy(&mut input, &mut conn);
        let _ = up.send(());
    });
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 1 << 16];
        loop {
            match back.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if output.write_all(&buf[..n]).and_then(|_| output.flush()).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = done.send(());
    });
    let _ = ended.recv();
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
        let Some(rem) = deadline.checked_duration_since(Instant::now()) else {
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
        let Some(rem) = deadline.checked_duration_since(Instant::now()) else {
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
/// Returns `Ok(true)` on full read, `Ok(false)` on clean EOF/stop before any byte was read,
/// or `Err` on partial EOF / deadline exceeded / I/O error.
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
            let Some(rem) = dl.checked_duration_since(Instant::now()) else {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "frame read timeout"));
            };
            rem.min(Duration::from_millis(100))
        } else {
            Duration::from_millis(150)
        };
        let _ = reader.get_ref().set_read_timeout(Some(timeout));
        match reader.read(&mut dst[off..]) {
            Ok(0) => {
                if off == 0 {
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

struct SpkStreamState {
    sample_rate: u32,
    ack_tx: Option<mpsc::Sender<(bool, String)>>,
    stop_flag: Arc<AtomicBool>,
    announced_to_epoch: Option<u64>,
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
    #[cfg_attr(not(test), allow(dead_code))]
    Shutdown,
}

struct ClientOut {
    epoch: u64,
    ssh: BufWriter<Box<dyn Write + Send>>,
    ssh_open: bool,
    ssh_closer: Box<dyn Fn() + Send>,
    ssh_closed_at: Option<Instant>,
    link: Option<Arc<UdpLink>>,
    ssh_client: Option<String>,
    test_hooks: bool,
}

impl ClientOut {
    fn udp_keeping_alive(&self) -> bool {
        self.link.as_ref().is_some_and(|l| {
            l.enabled.load(Ordering::Relaxed) && l.ever_received() && l.silent_us() <= UDP_KEEPALIVE_US
        })
    }

    fn is_connected(&self) -> bool {
        self.ssh_open || self.udp_keeping_alive()
    }

    fn emit(&mut self, t: u8, payload: &[u8]) {
        if let Some(l) = self.link.as_ref().filter(|l| l.alive()) {
            l.send_frame(t, payload);
            return;
        }
        if self.ssh_open && proto::write_frame(&mut self.ssh, t, payload).is_err() {
            self.ssh_open = false;
            self.ssh_closed_at.get_or_insert_with(Instant::now);
        }
    }

    fn flush(&mut self) {
        if self.ssh_open && self.ssh.flush().is_err() {
            self.ssh_open = false;
            self.ssh_closed_at.get_or_insert_with(Instant::now);
        }
    }

    fn close(mut self) {
        self.flush();
        if let Some(l) = self.link.take() {
            l.close();
        }
        (self.ssh_closer)();
    }
}

type UdpBinder = Box<dyn Fn(f64) -> Option<Arc<UdpLink>> + Send>;

struct VoiceEngine {
    mic_port: u16,
    session: Session,
    attached_once: bool,
    idle_timeout: Duration,
    bind_udp: UdpBinder,
    ev_tx: mpsc::Sender<Event>,
    client_connected: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    client: Option<ClientOut>,
    epoch: u64,
    disconnected_since: Option<Instant>,
    mic_streams: HashMap<u32, MicStreamHandle>,
    spk_streams: HashMap<u32, SpkStreamState>,
    evict_reply: Option<(mpsc::Sender<bool>, mpsc::Receiver<()>)>,
}

impl VoiceEngine {
    fn new(
        mic_port: u16,
        session: Session,
        idle_timeout: Duration,
        bind_udp: UdpBinder,
        ev_tx: mpsc::Sender<Event>,
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
        }
    }

    fn is_client_reachable(&self) -> bool {
        self.client
            .as_ref()
            .is_some_and(|c| c.ssh_open || c.link.as_ref().is_some_and(|l| l.alive()))
    }

    fn run(&mut self, ev_rx: mpsc::Receiver<Event>) -> Option<(mpsc::Sender<bool>, mpsc::Receiver<()>)> {
        loop {
            match ev_rx.recv_timeout(Duration::from_millis(40)) {
                Ok(ev) => {
                    if !self.handle_event(ev) {
                        break;
                    }
                    // Drain queued events before flushing so bursts land immediately.
                    let mut stop = false;
                    while let Ok(next) = ev_rx.try_recv() {
                        if !self.handle_event(next) {
                            stop = true;
                            break;
                        }
                    }
                    if let Some(c) = self.client.as_mut() {
                        c.flush();
                    }
                    if stop {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }

            if let Some(c) = self.client.as_mut() {
                if let Some(l) = &c.link {
                    l.tick();
                }
                if !c.is_connected() {
                    let old = self.client.take().unwrap();
                    old.close();
                    self.disconnected_since = Some(Instant::now());
                    crate::log!("voice client disconnected (session={})", self.session.session);
                }
            }
            self.client_connected.store(self.is_client_reachable(), Ordering::Relaxed);

            if self.client.is_none() {
                let wait = if self.attached_once {
                    self.idle_timeout
                } else {
                    // Initial grace period for the spawning `voice` CLI to attach over IPC.
                    self.idle_timeout.max(Duration::from_secs(10))
                };
                if let Some(since) = self.disconnected_since {
                    if since.elapsed() >= wait {
                        break;
                    }
                }
            }
        }

        self.shutdown.store(true, Ordering::SeqCst);
        self.client_connected.store(false, Ordering::Relaxed);
        if let Some(old) = self.client.take() {
            old.close();
        }
        for (_, m) in self.mic_streams.drain() {
            m.close();
        }
        for (_, s) in self.spk_streams.drain() {
            s.stop_flag.store(true, Ordering::SeqCst);
        }
        self.evict_reply.take()
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
                if self.mic_port == port && !self.is_client_reachable() {
                    crate::log!("voice-daemon evicted idle session={} mic_port={port}", self.session.session);
                    self.evict_reply = Some((reply_tx, flushed_rx));
                    return false;
                }
                let _ = reply_tx.send(false);
            }
            Event::ClientSshClosed { epoch } => {
                if let Some(c) = self.client.as_mut() {
                    if c.epoch == epoch {
                        c.ssh_open = false;
                        c.ssh_closed_at.get_or_insert_with(Instant::now);
                    }
                }
            }
            Event::AgyMicOpen { stream_id, mut handle } => {
                if let Some(c) = self.client.as_mut() {
                    c.emit(ty::MIC_START, &proto::mic_start_payload(stream_id, MIC_SAMPLE_RATE, CHANNELS));
                    handle.announced_to_epoch = Some(c.epoch);
                }
                self.mic_streams.insert(stream_id, handle);
            }
            Event::AgyMicClosed { stream_id } => {
                if let Some(handle) = self.mic_streams.remove(&stream_id) {
                    if let Some(c) = self.client.as_mut() {
                        if handle.announced_to_epoch == Some(c.epoch) {
                            c.emit(ty::MIC_STOP, &proto::mic_stop_payload(stream_id));
                        }
                    }
                }
            }
            Event::AgySpkOpen { stream_id, sample_rate, ack_tx, stop_flag } => {
                let mut state = SpkStreamState {
                    sample_rate,
                    ack_tx: Some(ack_tx),
                    stop_flag,
                    announced_to_epoch: None,
                };
                if let Some(c) = self.client.as_mut() {
                    c.emit(ty::SPK_START, &proto::spk_start_payload(stream_id, sample_rate, CHANNELS));
                    state.announced_to_epoch = Some(c.epoch);
                }
                self.spk_streams.insert(stream_id, state);
            }
            Event::AgySpkAudio { stream_id, pcm } => {
                if self.spk_streams.contains_key(&stream_id) {
                    if let Some(c) = self.client.as_mut() {
                        c.emit(ty::SPK_AUDIO, &proto::spk_audio_payload(stream_id, &pcm));
                    }
                }
            }
            Event::AgySpkClear { stream_id } => {
                if self.spk_streams.contains_key(&stream_id) {
                    if let Some(c) = self.client.as_mut() {
                        c.emit(ty::SPK_CLEAR, &proto::spk_clear_payload(stream_id));
                    }
                }
            }
            Event::AgySpkClosed { stream_id, notify_client } => {
                if let Some(state) = self.spk_streams.remove(&stream_id) {
                    if notify_client {
                        if let Some(c) = self.client.as_mut() {
                            if state.announced_to_epoch == Some(c.epoch) {
                                c.emit(ty::SPK_STOP, &proto::spk_stop_payload(stream_id));
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
        mut output: Box<dyn Write + Send>,
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
            // Valid resume silently replaces the prior controller.
            old.close();
        }
        if let Some(secs) = req.idle_timeout {
            self.idle_timeout = Duration::from_secs(secs);
        }

        // Confirm attach to the IPC handler BEFORE writing `VoiceHello` so `voice::cmd` sees `OK\n` first.
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
            codecs: vec!["pcm16".into()],
        };

        if writeln!(output, "{}", serde_json::to_string(&hello).unwrap())
            .and_then(|_| output.flush())
            .is_err()
        {
            if let Some(l) = link {
                l.close();
            }
            closer();
            return;
        }

        self.attached_once = true;
        self.disconnected_since = None;
        self.epoch += 1;
        let epoch = self.epoch;

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

        let mut out = ClientOut {
            epoch,
            ssh: BufWriter::with_capacity(64 * 1024, output),
            ssh_open: true,
            ssh_closer: closer,
            ssh_closed_at: None,
            link,
            ssh_client: req.ssh_client,
            test_hooks: std::env::var("ROAMTTY_DESKTOP_TEST").as_deref() == Ok("1"),
        };

        // Re-announce any still-open `agy` mic/speaker streams so a resumed app picks them up seamlessly.
        let mut mic_ids: Vec<u32> = self.mic_streams.keys().copied().collect();
        mic_ids.sort_unstable();
        for id in mic_ids {
            if let Some(m) = self.mic_streams.get_mut(&id) {
                out.emit(ty::MIC_START, &proto::mic_start_payload(id, MIC_SAMPLE_RATE, CHANNELS));
                m.announced_to_epoch = Some(epoch);
            }
        }
        let mut spk_ids: Vec<u32> = self.spk_streams.keys().copied().collect();
        spk_ids.sort_unstable();
        for id in spk_ids {
            if let Some(s) = self.spk_streams.get_mut(&id) {
                out.emit(ty::SPK_START, &proto::spk_start_payload(id, s.sample_rate, CHANNELS));
                s.announced_to_epoch = Some(epoch);
            }
        }
        out.flush();
        self.client = Some(out);
        self.client_connected.store(true, Ordering::Relaxed);
    }

    fn handle_client_msg(&mut self, msg: ClientMsg) -> bool {
        match msg {
            ClientMsg::Config(c) => {
                if c.shutdown == Some(true) {
                    crate::log!("voice client requested shutdown (session={})", self.session.session);
                    return false;
                }
                if let Some(out) = self.client.as_mut() {
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
                    out.emit(ty::PONG, &proto::pong_payload(t1, t2, crate::serve::now_us()));
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

fn spawn_mic_accept_loop(
    listener: TcpListener,
    next_stream_id: Arc<AtomicU32>,
    ev_tx: mpsc::Sender<Event>,
    client_connected: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    let _ = listener.set_nonblocking(true);
    std::thread::spawn(move || {
        while !shutdown.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_nodelay(true);
                    let next_stream_id = next_stream_id.clone();
                    let ev_tx = ev_tx.clone();
                    let client_connected = client_connected.clone();
                    let shutdown = shutdown.clone();
                    std::thread::spawn(move || {
                        handle_agy_conn(stream, &next_stream_id, ev_tx, client_connected, shutdown)
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    })
}

fn write_spk_err_and_close(stream: &mut TcpStream, reason: &str) {
    let _ = stream.set_write_timeout(Some(MIC_WRITE_TIMEOUT));
    let _ = stream.write_all(&format_spk_err(reason));
    let _ = stream.flush();
    // Half-close write and drain any unread bytes (such as peeked invalid headers) before drop
    // so peers (especially Winsock) receive the ERR line followed by clean FIN instead of RST.
    let _ = stream.shutdown(Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    let mut drain = [0u8; 512];
    while matches!(stream.read(&mut drain), Ok(n) if n > 0) {}
}

fn handle_agy_conn(
    mut stream: TcpStream,
    next_stream_id: &AtomicU32,
    ev_tx: mpsc::Sender<Event>,
    client_connected: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) {
    match classify_connection(&stream) {
        ConnKind::Eof => {
            let _ = stream.shutdown(Shutdown::Both);
        }
        ConnKind::Invalid => {
            write_spk_err_and_close(&mut stream, "invalid header");
        }
        ConnKind::Speaker => handle_speaker_conn(stream, next_stream_id, ev_tx, client_connected, shutdown),
        ConnKind::Mic => handle_mic_conn(stream, next_stream_id, ev_tx, shutdown),
    }
}

fn handle_speaker_conn(
    stream: TcpStream,
    next_stream_id: &AtomicU32,
    ev_tx: mpsc::Sender<Event>,
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
            write_spk_err_and_close(&mut write_half, reason);
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
        write_spk_err_and_close(&mut write_half, "bridge unavailable");
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
            write_spk_err_and_close(&mut write_half, &err);
            let _ = ev_tx.send(Event::AgySpkClosed { stream_id, notify_client: false });
            return;
        }
        Err(_) => {
            let reason = if client_connected.load(Ordering::Relaxed) {
                "speaker ack timeout"
            } else {
                "no voice client connected"
            };
            write_spk_err_and_close(&mut write_half, reason);
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
                if len % 2 != 0 {
                    crate::log!("voice speaker {stream_id} odd-length 'A' frame len={len}");
                    break;
                }
                let mut payload = vec![0u8; len];
                let deadline = Instant::now() + SPK_FRAME_READ_TIMEOUT;
                match read_exact_interruptible(&mut reader, &mut payload, &stop_flag, &shutdown, Some(deadline)) {
                    Ok(true) => {
                        for chunk in payload.chunks(MAX_SPK_AUDIO_CHUNK) {
                            if ev_tx.send(Event::AgySpkAudio { stream_id, pcm: chunk.to_vec() }).is_err() {
                                break 'frames;
                            }
                        }
                    }
                    Ok(false) | Err(_) => break,
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
    ev_tx: mpsc::Sender<Event>,
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
    let notified = Arc::new(AtomicBool::new(false));
    let handle = MicStreamHandle {
        queue: queue.clone(),
        sock: close_stream,
        announced_to_epoch: None,
    };

    if ev_tx.send(Event::AgyMicOpen { stream_id, handle }).is_err() {
        let _ = read_stream.shutdown(Shutdown::Both);
        return;
    }

    // Writer thread: forwards MIC_AUDIO PCM chunks to `agy` with a 5s write deadline.
    let w_queue = queue.clone();
    let w_notified = notified.clone();
    let w_ev_tx = ev_tx.clone();
    let writer_thread = std::thread::spawn(move || {
        let (lock, cvar) = &*w_queue;
        let mut had_error = false;
        loop {
            let chunk = {
                let mut q = lock.lock().unwrap();
                while q.chunks.is_empty() && !q.closed {
                    q = cvar.wait(q).unwrap();
                }
                if q.closed {
                    break;
                }
                q.chunks.pop_front()
            };
            let Some(pcm) = chunk else {
                continue;
            };
            if let Err(e) = write_stream.write_all(&pcm) {
                crate::log!("voice mic {stream_id} write error: {e}");
                had_error = true;
                let mut q = lock.lock().unwrap();
                q.closed = true;
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
    let (lock, cvar) = &*queue;
    loop {
        if shutdown.load(Ordering::Relaxed) || lock.lock().unwrap().closed {
            break;
        }
        match read_stream.read(&mut buf) {
            Ok(0) => break, // `agy` half-closed write end (FIN) to stop recording.
            Ok(_) => {}     // Ignore unexpected bytes from peer on mic socket.
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
    fn close(&self);
}

fn spawn_ipc_accept_loop(
    listener: Box<dyn Iterator<Item = Box<dyn IpcConn>> + Send>,
    ipc_token: String,
    ev_tx: mpsc::Sender<Event>,
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
                        _ => {
                            let _ = writeln!(status_writer, "BUSY");
                            let _ = status_writer.flush();
                            status_writer.close();
                        }
                    }
                    return;
                }
                let (reply_tx, reply_rx) = mpsc::channel::<bool>();
                // Write `OK\n` only if authorized, BEFORE `VoiceEngine` writes `VoiceHello`.
                // To preserve strict ordering between `OK\n` and `VoiceHello`, we wrap output so
                // `OK\n` is written synchronously when `reply_tx.send(true)` is invoked? Even simpler:
                // wait on a handshake channel — or have `VoiceEngine` write `OK\n` via a tiny wrapper?
                // Wait: `VoiceEngine` writes `VoiceHello` right after `reply_tx.send(true)`, which
                // could race with another thread writing `OK\n`! So wrap `output` to emit `OK\n`
                // on first write, guaranteeing `OK\n` ALWAYS precedes `VoiceHello`!
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

#[cfg(unix)]
mod ipc {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};

    // macOS `sockaddr_un.sun_path` is 104 bytes (103 + NUL). If `ROAMTTY_DESKTOP_ROOT` is long
    // (e.g. macOS `$TMPDIR`), fall back transparently to loopback TCP authenticated by the 0600 token.
    const MAX_UNIX_PATH_BYTES: usize = 96;

    fn sock_path(sess: &str) -> PathBuf {
        voice_dir().join(format!("{sess}.sock"))
    }
    fn port_path(sess: &str) -> PathBuf {
        voice_dir().join(format!("{sess}.ipc"))
    }
    fn token_path(sess: &str) -> PathBuf {
        voice_dir().join(format!("{sess}.token"))
    }

    impl IpcConn for UnixStream {
        fn split(&self) -> io::Result<Box<dyn IpcConn>> {
            Ok(Box::new(self.try_clone()?))
        }
        fn set_io_timeout(&self, dur: Option<Duration>) {
            let _ = self.set_read_timeout(dur);
            let _ = self.set_write_timeout(dur);
        }
        fn close(&self) {
            let _ = self.shutdown(Shutdown::Both);
        }
    }

    impl IpcConn for TcpStream {
        fn split(&self) -> io::Result<Box<dyn IpcConn>> {
            Ok(Box::new(self.try_clone()?))
        }
        fn set_io_timeout(&self, dur: Option<Duration>) {
            let _ = self.set_read_timeout(dur);
            let _ = self.set_write_timeout(dur);
        }
        fn close(&self) {
            let _ = self.shutdown(Shutdown::Both);
        }
    }

    pub fn read_token(sess: &str) -> Option<String> {
        std::fs::read_to_string(token_path(sess)).ok()
    }

    pub fn connect(sess: &str) -> io::Result<Box<dyn IpcConn>> {
        let sp = sock_path(sess);
        if sp.exists() {
            return Ok(Box::new(UnixStream::connect(&sp)?));
        }
        let port: u16 = std::fs::read_to_string(port_path(sess))?
            .trim()
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad ipc port"))?;
        let s = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(300))?;
        let _ = s.set_nodelay(true);
        Ok(Box::new(s))
    }

    pub fn listen(sess: &str, token: &str) -> io::Result<Box<dyn Iterator<Item = Box<dyn IpcConn>> + Send>> {
        paths::write_private(&token_path(sess), token.as_bytes())?;
        let sp = sock_path(sess);
        let _ = std::fs::remove_file(&sp);
        let _ = std::fs::remove_file(port_path(sess));
        if sp.as_os_str().len() <= MAX_UNIX_PATH_BYTES {
            let old = unsafe { libc::umask(0o077) };
            let bound = UnixListener::bind(&sp);
            unsafe { libc::umask(old); }
            if let Ok(l) = bound {
                return Ok(Box::new(std::iter::from_fn(move || loop {
                    match l.accept() {
                        Ok((s, _)) => return Some(Box::new(s) as Box<dyn IpcConn>),
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => return None,
                    }
                })));
            }
        }
        let l = TcpListener::bind(("127.0.0.1", 0))?;
        let port = l.local_addr()?.port();
        paths::write_private(&port_path(sess), port.to_string().as_bytes())?;
        Ok(Box::new(std::iter::from_fn(move || loop {
            match l.accept() {
                Ok((s, _)) => {
                    let _ = s.set_nodelay(true);
                    return Some(Box::new(s) as Box<dyn IpcConn>);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return None,
            }
        })))
    }

    pub fn cleanup(sess: &str) {
        let _ = std::fs::remove_file(sock_path(sess));
        let _ = std::fs::remove_file(port_path(sess));
        let _ = std::fs::remove_file(token_path(sess));
    }
}

#[cfg(windows)]
mod ipc {
    use super::*;

    fn port_path(sess: &str) -> PathBuf {
        voice_dir().join(format!("{sess}.ipc"))
    }
    fn token_path(sess: &str) -> PathBuf {
        voice_dir().join(format!("{sess}.token"))
    }

    impl IpcConn for TcpStream {
        fn split(&self) -> io::Result<Box<dyn IpcConn>> {
            Ok(Box::new(self.try_clone()?))
        }
        fn set_io_timeout(&self, dur: Option<Duration>) {
            let _ = self.set_read_timeout(dur);
            let _ = self.set_write_timeout(dur);
        }
        fn close(&self) {
            let _ = self.shutdown(Shutdown::Both);
        }
    }

    pub fn read_token(sess: &str) -> Option<String> {
        std::fs::read_to_string(token_path(sess)).ok()
    }

    pub fn connect(sess: &str) -> io::Result<Box<dyn IpcConn>> {
        let port: u16 = std::fs::read_to_string(port_path(sess))?
            .trim()
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad ipc port"))?;
        let s = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(300))?;
        let _ = s.set_nodelay(true);
        Ok(Box::new(s))
    }

    pub fn listen(sess: &str, token: &str) -> io::Result<Box<dyn Iterator<Item = Box<dyn IpcConn>> + Send>> {
        paths::write_private(&token_path(sess), token.as_bytes())?;
        let l = TcpListener::bind(("127.0.0.1", 0))?;
        let port = l.local_addr()?.port();
        paths::write_private(&port_path(sess), port.to_string().as_bytes())?;
        Ok(Box::new(std::iter::from_fn(move || loop {
            match l.accept() {
                Ok((s, _)) => {
                    let _ = s.set_nodelay(true);
                    return Some(Box::new(s) as Box<dyn IpcConn>);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return None,
            }
        })))
    }

    pub fn cleanup(sess: &str) {
        let _ = std::fs::remove_file(port_path(sess));
        let _ = std::fs::remove_file(token_path(sess));
    }
}

#[cfg(test)]
#[path = "voice_tests.rs"]
mod tests;
