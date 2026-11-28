#![cfg_attr(not(unix), allow(dead_code))]
//! Isolated Codex CLI voice backend for `roamtty-desktop voice --backend codex`.
//!
//! Bridges Roamtty's existing `VoiceEngine` (Opus+RED+JitterBuffer over `UdpLink`/SSH)
//! directly to OpenAI Codex CLI's shared `codex app-server` daemon (`app-server-control.sock`)
//! using `thread/realtime/start` (`transport: "webrtc"`, `version: "v3"`) and pure-Rust
//! Sans-I/O WebRTC (`str0m` + `str0m-rust-crypto`).
//!
//! Strictly isolated from `agy` (`JETSKI-AUDIO/1`): binds no loopback TCP port, touches no
//! `ANTIGRAVITY_MIC` state, and sends zero terminal hotkeys.

use crate::voice::{Event, MicQueue, MicStreamHandle, MIC_SAMPLE_RATE};
use crate::voice_codec::{
    frame_samples, i16_to_s16le_bytes, s16le_bytes_to_i16, VoiceJitterBuffer, VoiceOpusDecoder,
    VoiceOpusEncoder,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const SPK_SAMPLE_RATE: u32 = 24_000;
const WEBRTC_SAMPLE_RATE: u32 = 48_000;
const WEBRTC_BITRATE: i32 = 32_000;
const SPK_SILENCE_RMS: f32 = 0.002;
const SPK_TAIL_FRAMES: u32 = 25; // 500 ms hangover bridges natural inter-clause pauses without PLC clicks
const SPK_OPUS_SILENCE_MAX_BYTES: usize = 8;
const SPK_FADE_SAMPLES: usize = 120; // 5 ms @ 24 kHz raised-cosine fade-in / fade-out
const SPK_HEADROOM_GAIN: f32 = 0.84; // -1.5 dB headroom prevents double-Opus peak clipping
const SPK_LIMITER_KNEE: f32 = 0.85;
const SPK_LIMITER_CEIL: f32 = 0.98;
#[cfg(unix)]
const SPK_OUT_QUEUE_MAX: usize = 32; // 640 ms local buffer before ev_tx so bursts never drop frames
const RTP_FRAME_TICKS_48K: u64 = 960; // 20 ms @ 48 kHz
const RTP_MAX_CONCEAL_TICKS_48K: i64 = 12_000; // 250 ms max gap concealed as loss (larger = talkspurt pause)
const RTP_MAX_CONCEAL_FRAMES: usize = 5;
const RTP_LATE_WINDOW_TICKS_48K: i64 = -(48_000 * 5);

/// Configuration parsed from `roamtty-desktop voice --backend codex ...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexConfig {
    pub thread_id: Option<String>,
    pub cwd: Option<String>,
    pub sock_path: Option<String>,
    pub voice: Option<String>,
}

pub fn is_valid_thread_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn is_valid_voice_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Smooth 3x linear upsampler (16 kHz -> 48 kHz) maintaining cross-frame sample continuity.
pub(crate) struct Upsampler16To48 {
    prev: i16,
}

impl Upsampler16To48 {
    pub(crate) fn new() -> Self {
        Self { prev: 0 }
    }

    /// Converts 320 samples (20 ms @ 16 kHz) into 960 samples (20 ms @ 48 kHz).
    pub(crate) fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        out.clear();
        out.reserve(input.len() * 3);
        let mut prev = self.prev as i32;
        for &sample in input {
            let cur = sample as i32;
            let s1 = prev + (cur - prev) / 3;
            let s2 = prev + ((cur - prev) * 2) / 3;
            out.push(s1.clamp(i16::MIN as i32, i16::MAX as i32) as i16);
            out.push(s2.clamp(i16::MIN as i32, i16::MAX as i32) as i16);
            out.push(sample);
            prev = cur;
        }
        self.prev = prev as i16;
    }
}

/// Classifies OpenAI Realtime / Codex `oai-events` and `app-server` JSON events
/// for speaker barge-in (`SPK_CLEAR`) tracking.
#[derive(Debug, Default)]
pub(crate) struct BargeInTracker {
    assistant_active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BargeInAction {
    None,
    ClearSpeaker,
}

impl BargeInTracker {
    pub(crate) fn on_speaker_speech(&mut self) {
        self.assistant_active = true;
    }

    pub(crate) fn on_oai_event(&mut self, v: &Value) -> BargeInAction {
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        match ty {
            "input_audio_buffer.speech_started" | "response.cancelled" => {
                let clear = self.assistant_active;
                self.assistant_active = false;
                if clear {
                    BargeInAction::ClearSpeaker
                } else {
                    BargeInAction::None
                }
            }
            "turn.created" => {
                let role = v
                    .get("turn")
                    .and_then(|t| t.get("role"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if role == "assistant" {
                    self.assistant_active = true;
                    BargeInAction::None
                } else if role == "user" && self.assistant_active {
                    self.assistant_active = false;
                    BargeInAction::ClearSpeaker
                } else {
                    BargeInAction::None
                }
            }
            "turn.done" => {
                let role = v
                    .get("turn")
                    .and_then(|t| t.get("role"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if role == "assistant" {
                    self.assistant_active = false;
                }
                BargeInAction::None
            }
            "input_transcript.added" => {
                if self.assistant_active {
                    self.assistant_active = false;
                    BargeInAction::ClearSpeaker
                } else {
                    BargeInAction::None
                }
            }
            "output_transcript.added" => {
                self.assistant_active = true;
                BargeInAction::None
            }
            _ => BargeInAction::None,
        }
    }

    pub(crate) fn on_app_server_notification(&mut self, method: &str, params: &Value) -> BargeInAction {
        match method {
            "thread/realtime/itemAdded" => {
                if let Some(item) = params.get("item") {
                    return self.on_oai_event(item);
                }
                BargeInAction::None
            }
            "thread/realtime/transcript/delta" => {
                let role = params.get("role").and_then(Value::as_str).unwrap_or("");
                if role == "assistant" {
                    self.assistant_active = true;
                    BargeInAction::None
                } else if role == "user" && self.assistant_active {
                    self.assistant_active = false;
                    BargeInAction::ClearSpeaker
                } else {
                    BargeInAction::None
                }
            }
            _ => BargeInAction::None,
        }
    }
}

fn f32_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sum = 0.0f64;
    for &s in samples {
        let f = s as f64;
        sum += f * f;
    }
    (sum / samples.len() as f64).sqrt() as f32
}

/// Click-free gate decision for continuous WebRTC speaker audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GateDecision {
    Suppress,
    FadeIn,
    Pass,
    FadeOut,
}

/// Speaker comfort-silence suppressor for continuous WebRTC Opus streams with 5 ms fade edges.
#[derive(Debug, Default)]
pub(crate) struct SpeakerSilenceGate {
    active: bool,
    tail_remaining: u32,
}

impl SpeakerSilenceGate {
    /// Classifies this decoded frame as suppressed silence, fade-in attack, normal pass, or fade-out tail.
    pub(crate) fn accept(&mut self, opus_len: usize, rms: f32) -> GateDecision {
        if opus_len > SPK_OPUS_SILENCE_MAX_BYTES || rms >= SPK_SILENCE_RMS {
            let was_active = self.active;
            self.active = true;
            self.tail_remaining = SPK_TAIL_FRAMES;
            if was_active {
                GateDecision::Pass
            } else {
                GateDecision::FadeIn
            }
        } else if self.tail_remaining > 1 {
            self.tail_remaining -= 1;
            GateDecision::Pass
        } else if self.tail_remaining == 1 {
            self.tail_remaining = 0;
            self.active = false;
            GateDecision::FadeOut
        } else {
            self.active = false;
            GateDecision::Suppress
        }
    }

    pub(crate) fn reset(&mut self) {
        self.active = false;
        self.tail_remaining = 0;
    }
}

/// Downlink floating-point DSP for Codex speaker audio:
/// 1. 20 Hz DC-blocker (prevents DC-offset thumps on gate/barge-in edges)
/// 2. -1.5 dB headroom + smooth tanh soft-knee limiter (eliminates double-Opus hard clipping)
/// 3. 5 ms raised-cosine fade-in / fade-out on silence-gate edges (eliminates step clicks)
#[derive(Debug, Default)]
pub(crate) struct SpeakerDsp {
    dc_prev_x: f32,
    dc_prev_y: f32,
}

impl SpeakerDsp {
    pub(crate) fn process_to_i16(&mut self, input: &[f32], decision: GateDecision, out: &mut Vec<i16>) {
        out.clear();
        out.reserve(input.len());
        let n = input.len();
        let fade_len = SPK_FADE_SAMPLES.min(n);
        let knee = SPK_LIMITER_KNEE;
        let span = SPK_LIMITER_CEIL - SPK_LIMITER_KNEE;
        let mut prev_x = self.dc_prev_x;
        let mut prev_y = self.dc_prev_y;

        for (i, &raw) in input.iter().enumerate() {
            let x = if raw.is_finite() { raw } else { 0.0 };
            let dc = x - prev_x + 0.995 * prev_y;
            prev_x = x;
            prev_y = dc;

            let scaled = dc * SPK_HEADROOM_GAIN;
            let abs = scaled.abs();
            let mut limited = if abs > knee {
                let sign = if scaled >= 0.0 { 1.0 } else { -1.0 };
                sign * (knee + span * ((abs - knee) / span).tanh())
            } else {
                scaled
            };

            match decision {
                GateDecision::FadeIn if fade_len > 0 && i < fade_len => {
                    let w = 0.5 * (1.0 - (std::f32::consts::PI * i as f32 / fade_len as f32).cos());
                    limited *= w;
                }
                GateDecision::FadeOut if fade_len > 0 && i >= n - fade_len => {
                    let k = i - (n - fade_len);
                    let w = 0.5 * (1.0 + (std::f32::consts::PI * (k + 1) as f32 / fade_len as f32).cos());
                    limited *= w;
                }
                _ => {}
            }

            let sample = (limited * 32767.0).round().clamp(-32768.0, 32767.0) as i16;
            out.push(sample);
        }

        self.dc_prev_x = prev_x;
        self.dc_prev_y = prev_y;
    }

    pub(crate) fn reset(&mut self) {
        self.dc_prev_x = 0.0;
        self.dc_prev_y = 0.0;
    }
}

/// WebRTC RTP timestamp continuity tracker for incoming Codex audio frames (48 kHz clock).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RtpGapAction {
    Normal,
    DropLate,
    Conceal { plc_frames: usize, use_fec: bool },
}

#[derive(Debug, Default)]
pub(crate) struct RtpGapTracker {
    expected_time_48k: Option<u64>,
}

impl RtpGapTracker {
    pub(crate) fn on_packet(&self, rtp_time_48k: u64, start_of_talk_spurt: bool) -> RtpGapAction {
        let Some(expected) = self.expected_time_48k else {
            return RtpGapAction::Normal;
        };
        let delta = rtp_time_48k.wrapping_sub(expected) as i64;
        if delta < 0 && delta > RTP_LATE_WINDOW_TICKS_48K {
            return RtpGapAction::DropLate;
        }
        if start_of_talk_spurt || delta < (RTP_FRAME_TICKS_48K as i64 / 2) || delta > RTP_MAX_CONCEAL_TICKS_48K {
            return RtpGapAction::Normal;
        }
        let missing = ((delta as u64 + RTP_FRAME_TICKS_48K / 2) / RTP_FRAME_TICKS_48K)
            .clamp(1, RTP_MAX_CONCEAL_FRAMES as u64) as usize;
        RtpGapAction::Conceal {
            plc_frames: missing - 1,
            use_fec: true,
        }
    }

    pub(crate) fn advance(&mut self, rtp_time_48k: u64, decoded_samples_24k: usize) {
        self.expected_time_48k = Some(rtp_time_48k.wrapping_add((decoded_samples_24k as u64) * 2));
    }
}

#[cfg(unix)]
mod unix_impl {
    use super::*;
    use base64ct::{Base64, Encoding};
    use sha1::{Digest, Sha1};
    use std::io::{self, Read, Write};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid};
    use str0m::net::{Protocol, Receive};
    use str0m::{Candidate, Event as RtcEvent, Input, Output, Rtc, RtcConfig};

    const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    const RPC_TIMEOUT: Duration = Duration::from_secs(12);
    const WEBRTC_CONNECT_TIMEOUT: Duration = Duration::from_secs(12);
    const WEBRTC_DISCONNECT_GRACE: Duration = Duration::from_secs(6);

    pub(crate) struct UnixWsClient {
        stream: UnixStream,
        rx: Vec<u8>,
        frag_buf: Vec<u8>,
    }

    impl UnixWsClient {
        pub(crate) fn connect(path: &Path) -> io::Result<Self> {
            let mut stream = UnixStream::connect(path)?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            stream.set_write_timeout(Some(Duration::from_secs(5)))?;

            let mut rand_key = [0u8; 16];
            getrandom::fill(&mut rand_key).map_err(|e| io::Error::other(e.to_string()))?;
            let key_b64 = Base64::encode_string(&rand_key);
            let req = format!(
                "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key_b64}\r\nSec-WebSocket-Version: 13\r\n\r\n"
            );
            stream.write_all(req.as_bytes())?;
            stream.flush()?;

            let mut buf = Vec::with_capacity(512);
            let mut tmp = [0u8; 4096];
            let hdr_end = loop {
                let n = stream.read(&mut tmp)?;
                if n == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "codex app-server closed during WS handshake",
                    ));
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
                if buf.len() > 16 * 1024 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "oversized WS handshake response",
                    ));
                }
            };
            let hdr = std::str::from_utf8(&buf[..hdr_end]).unwrap_or("");
            if !hdr.lines().next().unwrap_or("").contains("101") {
                return Err(io::Error::other(format!(
                    "codex app-server WS upgrade rejected: {}",
                    hdr.lines().next().unwrap_or("")
                )));
            }
            let mut hasher = Sha1::new();
            hasher.update(key_b64.as_bytes());
            hasher.update(WS_GUID.as_bytes());
            let expected = Base64::encode_string(&hasher.finalize());
            if !hdr.contains(&expected) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Sec-WebSocket-Accept mismatch",
                ));
            }
            let rx = buf[hdr_end..].to_vec();
            stream.set_nonblocking(true)?;
            Ok(Self {
                stream,
                rx,
                frag_buf: Vec::new(),
            })
        }

        pub(crate) fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> io::Result<()> {
            let mut mask = [0u8; 4];
            getrandom::fill(&mut mask).map_err(|e| io::Error::other(e.to_string()))?;
            let mut frame = Vec::with_capacity(14 + payload.len());
            frame.push(0x80 | opcode);
            let n = payload.len();
            if n < 126 {
                frame.push(0x80 | (n as u8));
            } else if n < 65536 {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(n as u16).to_be_bytes());
            } else {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(n as u64).to_be_bytes());
            }
            frame.extend_from_slice(&mask);
            for (i, &b) in payload.iter().enumerate() {
                frame.push(b ^ mask[i & 3]);
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut off = 0usize;
            while off < frame.len() {
                match self.stream.write(&frame[off..]) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "codex WS write zero",
                        ));
                    }
                    Ok(w) => off += w,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted => {
                        if Instant::now() >= deadline {
                            return Err(io::Error::new(io::ErrorKind::TimedOut, "codex WS write timeout"));
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }

        pub(crate) fn send_json(&mut self, v: &Value) -> io::Result<()> {
            let s = serde_json::to_string(v).map_err(|e| io::Error::other(e.to_string()))?;
            self.send_frame(0x1, s.as_bytes())
        }

        pub(crate) fn poll_json(&mut self) -> io::Result<Option<Value>> {
            let mut eof = false;
            let mut tmp = [0u8; 65536];
            loop {
                match self.stream.read(&mut tmp) {
                    Ok(0) => {
                        eof = true;
                        break;
                    }
                    Ok(n) => self.rx.extend_from_slice(&tmp[..n]),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
            loop {
                if self.rx.len() < 2 {
                    return if eof {
                        Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "codex app-server disconnected",
                        ))
                    } else {
                        Ok(None)
                    };
                }
                let b0 = self.rx[0];
                let b1 = self.rx[1];
                let fin = (b0 & 0x80) != 0;
                let opcode = b0 & 0x0F;
                let masked = (b1 & 0x80) != 0;
                let mut plen = (b1 & 0x7F) as usize;
                let mut off = 2usize;
                if plen == 126 {
                    if self.rx.len() < off + 2 {
                        return if eof {
                            Err(io::Error::new(io::ErrorKind::UnexpectedEof, "codex app-server disconnected"))
                        } else {
                            Ok(None)
                        };
                    }
                    plen = u16::from_be_bytes([self.rx[off], self.rx[off + 1]]) as usize;
                    off += 2;
                } else if plen == 127 {
                    if self.rx.len() < off + 8 {
                        return if eof {
                            Err(io::Error::new(io::ErrorKind::UnexpectedEof, "codex app-server disconnected"))
                        } else {
                            Ok(None)
                        };
                    }
                    plen = u64::from_be_bytes(self.rx[off..off + 8].try_into().unwrap()) as usize;
                    off += 8;
                }
                if plen > 16 * 1024 * 1024 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "oversized codex WS frame",
                    ));
                }
                let mask = if masked {
                    if self.rx.len() < off + 4 {
                        return if eof {
                            Err(io::Error::new(io::ErrorKind::UnexpectedEof, "codex app-server disconnected"))
                        } else {
                            Ok(None)
                        };
                    }
                    let m = [self.rx[off], self.rx[off + 1], self.rx[off + 2], self.rx[off + 3]];
                    off += 4;
                    Some(m)
                } else {
                    None
                };
                if self.rx.len() < off + plen {
                    return if eof {
                        Err(io::Error::new(io::ErrorKind::UnexpectedEof, "codex app-server disconnected"))
                    } else {
                        Ok(None)
                    };
                }
                let mut payload = self.rx[off..off + plen].to_vec();
                self.rx.drain(..off + plen);
                if let Some(m) = mask {
                    for (i, b) in payload.iter_mut().enumerate() {
                        *b ^= m[i & 3];
                    }
                }
                match opcode {
                    0x9 => {
                        let _ = self.send_frame(0xA, &payload);
                    }
                    0xA => {}
                    0x8 => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "codex app-server sent WS close",
                        ));
                    }
                    0x1 | 0x2 | 0x0 => {
                        if opcode != 0x0 {
                            self.frag_buf.clear();
                        }
                        self.frag_buf.extend_from_slice(&payload);
                        if fin {
                            let full = std::mem::take(&mut self.frag_buf);
                            if let Ok(v) = serde_json::from_slice::<Value>(&full) {
                                return Ok(Some(v));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        pub(crate) fn close(&mut self) {
            let _ = self.send_frame(0x8, &[]);
            let _ = self.stream.shutdown(std::net::Shutdown::Both);
        }
    }

    pub(crate) struct PreparedCodexSession {
        ws: UnixWsClient,
        rpc_id: u64,
        thread_id: String,
        rtc: Rtc,
        udp: UdpSocket,
        local_addr: SocketAddr,
        audio_mid: Mid,
    }

    fn default_codex_socket_path() -> PathBuf {
        if let Ok(codex_home) = std::env::var("CODEX_HOME") {
            if !codex_home.trim().is_empty() {
                return PathBuf::from(codex_home)
                    .join("app-server-control")
                    .join("app-server-control.sock");
            }
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home)
            .join(".codex")
            .join("app-server-control")
            .join("app-server-control.sock")
    }

    fn connect_codex_ws(override_path: Option<&str>) -> Result<UnixWsClient, String> {
        let sock_path = override_path
            .map(PathBuf::from)
            .unwrap_or_else(default_codex_socket_path);
        if let Ok(ws) = UnixWsClient::connect(&sock_path) {
            return Ok(ws);
        }
        if override_path.is_none() {
            // Best-effort start of the shared Codex app-server daemon.
            let _ = std::process::Command::new("codex")
                .args(["app-server", "daemon", "start"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            let deadline = Instant::now() + Duration::from_secs(4);
            while Instant::now() < deadline {
                if let Ok(ws) = UnixWsClient::connect(&sock_path) {
                    return Ok(ws);
                }
                std::thread::sleep(Duration::from_millis(120));
            }
        }
        Err(format!(
            "unable to connect to codex app-server socket at {}",
            sock_path.display()
        ))
    }

    fn send_rpc(ws: &mut UnixWsClient, rpc_id: &mut u64, method: &str, params: Value) -> Result<u64, String> {
        *rpc_id += 1;
        let id = *rpc_id;
        ws.send_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .map_err(|e| format!("codex RPC send ({method}): {e}"))?;
        Ok(id)
    }

    fn wait_rpc_resp(
        ws: &mut UnixWsClient,
        expected_id: u64,
        timeout: Duration,
    ) -> Result<(Value, Vec<Value>), String> {
        let deadline = Instant::now() + timeout;
        let mut notifications = Vec::new();
        while Instant::now() < deadline {
            match ws.poll_json() {
                Ok(Some(v)) => {
                    if v.get("id").and_then(Value::as_u64) == Some(expected_id) {
                        if let Some(err) = v.get("error") {
                            let msg = err
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("codex RPC error");
                            return Err(msg.to_string());
                        }
                        return Ok((v.get("result").cloned().unwrap_or(Value::Null), notifications));
                    }
                    notifications.push(v);
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => return Err(format!("codex RPC read error: {e}")),
            }
        }
        Err(format!("codex RPC timed out (id={expected_id})"))
    }

    fn local_lan_ip() -> IpAddr {
        if let Ok(s) = UdpSocket::bind("0.0.0.0:0") {
            if s.connect("8.8.8.8:80").is_ok() {
                if let Ok(addr) = s.local_addr() {
                    return addr.ip();
                }
            }
        }
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    }

    fn canonical_path_str(p: &str) -> String {
        std::fs::canonicalize(p)
            .ok()
            .and_then(|c| c.into_os_string().into_string().ok())
            .unwrap_or_else(|| p.trim_end_matches('/').to_string())
    }

    fn resolve_thread_id(
        ws: &mut UnixWsClient,
        rpc_id: &mut u64,
        requested: Option<&str>,
        cwd_hint: Option<&str>,
    ) -> Result<String, String> {
        let list_id = send_rpc(ws, rpc_id, "thread/loaded/list", json!({}))?;
        let (list_res, _) = wait_rpc_resp(ws, list_id, RPC_TIMEOUT)?;
        let loaded: Vec<String> = list_res
            .get("data")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        if let Some(tid) = requested.filter(|s| !s.is_empty()) {
            if loaded.iter().any(|x| x == tid) {
                return Ok(tid.to_string());
            }
            // Thread may exist on disk but not yet loaded into the daemon; try thread/resume best-effort.
            let res_id = send_rpc(ws, rpc_id, "thread/resume", json!({ "threadId": tid }))?;
            match wait_rpc_resp(ws, res_id, RPC_TIMEOUT) {
                Ok(_) => return Ok(tid.to_string()),
                Err(e) => {
                    return Err(format!(
                        "codex thread {tid} is not attached to app-server daemon ({e}); ensure Codex started without -c overrides"
                    ));
                }
            }
        }

        if loaded.len() == 1 {
            return Ok(loaded.into_iter().next().unwrap());
        }
        if loaded.is_empty() {
            return Err(
                "no active Codex session found on app-server daemon; send one prompt or run /status in Codex first"
                    .into(),
            );
        }
        if let Some(hint) = cwd_hint.filter(|s| !s.is_empty()) {
            let want = canonical_path_str(hint);
            let mut best: Option<(i64, String)> = None;
            for tid in &loaded {
                let read_id = send_rpc(ws, rpc_id, "thread/read", json!({ "threadId": tid }))?;
                if let Ok((res, _)) = wait_rpc_resp(ws, read_id, Duration::from_secs(3)) {
                    let thread = res.get("thread").unwrap_or(&res);
                    let t_cwd = thread.get("cwd").and_then(Value::as_str).unwrap_or("");
                    if !t_cwd.is_empty() && (t_cwd == hint || canonical_path_str(t_cwd) == want) {
                        let updated = thread.get("updatedAt").and_then(Value::as_i64).unwrap_or(0);
                        if best.as_ref().is_none_or(|(b_ts, _)| updated >= *b_ts) {
                            best = Some((updated, tid.clone()));
                        }
                    }
                }
            }
            if let Some((_, tid)) = best {
                return Ok(tid);
            }
        }
        Err(format!(
            "found {} active Codex sessions on daemon without --codex-thread; send a message in this Codex session or pass --codex-thread",
            loaded.len()
        ))
    }

    /// Connects to Codex `app-server`, resolves the target thread, and completes SDP signaling
    /// BEFORE `voice-daemon` signals readiness to the mobile app.
    pub(crate) fn prepare(cfg: &CodexConfig) -> Result<PreparedCodexSession, String> {
        let mut ws = connect_codex_ws(cfg.sock_path.as_deref())?;
        let mut rpc_id = 0u64;

        let init_id = send_rpc(
            &mut ws,
            &mut rpc_id,
            "initialize",
            json!({
                "clientInfo": {
                    "name": "roamtty-desktop",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": { "experimentalApi": true },
            }),
        )?;
        let _ = wait_rpc_resp(&mut ws, init_id, RPC_TIMEOUT)?;
        ws.send_json(&json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {},
        }))
        .map_err(|e| format!("codex initialized notify: {e}"))?;

        let thread_id = resolve_thread_id(
            &mut ws,
            &mut rpc_id,
            cfg.thread_id.as_deref(),
            cfg.cwd.as_deref(),
        )?;

        // Build pure-Rust str0m WebRTC peer & generate SDP Offer.
        let udp = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind codex webrtc udp: {e}"))?;
        udp.set_nonblocking(true)
            .map_err(|e| format!("set nonblocking udp: {e}"))?;
        let local_port = udp.local_addr().map_err(|e| e.to_string())?.port();
        let local_addr = SocketAddr::new(local_lan_ip(), local_port);

        let provider = str0m_rust_crypto::default_provider();
        let mut rtc = RtcConfig::new()
            .set_crypto_provider(provider.into())
            .clear_codecs()
            .enable_opus(true, true)
            .build(Instant::now());
        let host_cand = Candidate::host(local_addr, Protocol::Udp)
            .map_err(|e| format!("invalid local host candidate: {e}"))?;
        rtc.add_local_candidate(host_cand);

        let mut change = rtc.sdp_api();
        let audio_mid = change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
        let _dc_id = change.add_channel("oai-events".into());
        let (offer, pending) = change
            .apply()
            .ok_or_else(|| "str0m failed to generate SDP offer".to_string())?;
        let offer_sdp = offer.to_sdp_string();

        let mut start_params = json!({
            "threadId": thread_id,
            "transport": { "type": "webrtc", "sdp": offer_sdp },
            "version": "v3",
            "outputModality": "audio",
        });
        if let Some(voice) = cfg.voice.as_deref() {
            start_params["voice"] = Value::String(voice.to_string());
        }

        let start_id = send_rpc(&mut ws, &mut rpc_id, "thread/realtime/start", start_params)?;
        let (_, early_notes) = wait_rpc_resp(&mut ws, start_id, RPC_TIMEOUT)?;

        // Locate thread/realtime/sdp (may arrive before or right after start response).
        let mut answer_sdp: Option<String> = None;
        for note in early_notes {
            if let Some(sdp) = extract_sdp_or_err(&note, &thread_id)? {
                answer_sdp = Some(sdp);
                break;
            }
        }
        let deadline = Instant::now() + RPC_TIMEOUT;
        while answer_sdp.is_none() {
            if Instant::now() >= deadline {
                let _ = send_rpc(
                    &mut ws,
                    &mut rpc_id,
                    "thread/realtime/stop",
                    json!({ "threadId": thread_id }),
                );
                return Err("timed out waiting for Codex WebRTC SDP answer".into());
            }
            match ws.poll_json() {
                Ok(Some(v)) => {
                    if let Some(sdp) = extract_sdp_or_err(&v, &thread_id)? {
                        answer_sdp = Some(sdp);
                    }
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => return Err(format!("codex WS closed during SDP wait: {e}")),
            }
        }

        let answer = str0m::change::SdpAnswer::from_sdp_string(&answer_sdp.unwrap())
            .map_err(|e| format!("invalid Codex SDP answer: {e}"))?;
        rtc.sdp_api()
            .accept_answer(pending, answer)
            .map_err(|e| format!("str0m accept_answer failed: {e}"))?;

        crate::log!("voice-codex prepared thread={thread_id} local_udp={local_addr}");
        Ok(PreparedCodexSession {
            ws,
            rpc_id,
            thread_id,
            rtc,
            udp,
            local_addr,
            audio_mid,
        })
    }

    fn extract_sdp_or_err(v: &Value, thread_id: &str) -> Result<Option<String>, String> {
        let meth = v.get("method").and_then(Value::as_str).unwrap_or("");
        let params = v.get("params").unwrap_or(&Value::Null);
        let note_tid = params.get("threadId").and_then(Value::as_str);
        if note_tid.is_some() && note_tid != Some(thread_id) {
            return Ok(None);
        }
        match meth {
            "thread/realtime/sdp" => Ok(params.get("sdp").and_then(Value::as_str).map(String::from)),
            "thread/realtime/error" => {
                let msg = params
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Codex realtime error");
                Err(msg.to_string())
            }
            "thread/realtime/closed" => {
                let reason = params
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("closed");
                Err(format!("Codex realtime closed during startup: {reason}"))
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn spawn_backend(
        prepared: PreparedCodexSession,
        next_stream_id: Arc<AtomicU32>,
        ev_tx: mpsc::SyncSender<Event>,
        shutdown: Arc<AtomicBool>,
    ) -> JoinHandle<()> {
        std::thread::Builder::new()
            .name("voice-codex".into())
            .spawn(move || run_loop(prepared, next_stream_id, ev_tx, shutdown))
            .expect("spawn voice-codex thread")
    }

    fn run_loop(
        mut sess: PreparedCodexSession,
        next_stream_id: Arc<AtomicU32>,
        ev_tx: mpsc::SyncSender<Event>,
        shutdown: Arc<AtomicBool>,
    ) {
        let mut mic_enc = match VoiceOpusEncoder::new(WEBRTC_SAMPLE_RATE, WEBRTC_BITRATE, false) {
            Ok(e) => e,
            Err(e) => {
                crate::log!("voice-codex opus encoder init failed: {e}");
                let _ = send_stop(&mut sess);
                let _ = ev_tx.send(Event::Shutdown);
                return;
            }
        };
        let mut spk_dec = match VoiceOpusDecoder::new(SPK_SAMPLE_RATE) {
            Ok(d) => d,
            Err(e) => {
                crate::log!("voice-codex opus decoder init failed: {e}");
                let _ = send_stop(&mut sess);
                let _ = ev_tx.send(Event::Shutdown);
                return;
            }
        };

        let mic_stream_id = next_stream_id.fetch_add(1, Ordering::Relaxed);
        let spk_stream_id = next_stream_id.fetch_add(1, Ordering::Relaxed);

        let mic_queue = Arc::new((
            Mutex::new(MicQueue {
                chunks: VecDeque::new(),
                closed: false,
            }),
            Condvar::new(),
        ));
        let mic_opus_jb: Arc<Mutex<Option<VoiceJitterBuffer>>> = Arc::new(Mutex::new(None));
        let spk_stop_flag = Arc::new(AtomicBool::new(false));
        let local_stop = Arc::new(AtomicBool::new(false));
        let (spk_ack_tx, spk_ack_rx) = mpsc::channel::<(bool, String)>();

        let mut streams_opened = false;
        let mut webrtc_connected = false;
        let mut disconnected_at: Option<Instant> = None;
        let connect_deadline = Instant::now() + WEBRTC_CONNECT_TIMEOUT;

        let mut upsampler = Upsampler16To48::new();
        let mut mic_pcm16_pending: Vec<i16> = Vec::with_capacity(960);
        let mut mic_pcm16_scratch: Vec<i16> = Vec::with_capacity(640);
        let mut mic_pcm48: Vec<i16> = Vec::with_capacity(960);
        let silence_48k = vec![0i16; frame_samples(WEBRTC_SAMPLE_RATE)];
        let mic_frame_16k = frame_samples(MIC_SAMPLE_RATE);

        let mut spk_float: Vec<f32> = Vec::with_capacity(frame_samples(SPK_SAMPLE_RATE) * 6);
        let mut spk_decoded: Vec<i16> = Vec::with_capacity(frame_samples(SPK_SAMPLE_RATE) * 6);
        let mut silence_gate = SpeakerSilenceGate::default();
        let mut spk_dsp = SpeakerDsp::default();
        let mut rtp_gap = RtpGapTracker::default();
        let mut spk_out_queue: VecDeque<Vec<u8>> = VecDeque::with_capacity(SPK_OUT_QUEUE_MAX);
        let mut barge_tracker = BargeInTracker::default();

        let mut rtp_ts: u64 = 0;
        let mut next_tx_at = Instant::now();
        let mut udp_buf = vec![0u8; 4096];

        let flush_spk_queue = |queue: &mut VecDeque<Vec<u8>>, local_stop: &AtomicBool| {
            while let Some(pcm) = queue.pop_front() {
                match ev_tx.try_send(Event::AgySpkAudio {
                    stream_id: spk_stream_id,
                    pcm,
                }) {
                    Ok(()) => {}
                    Err(mpsc::TrySendError::Full(Event::AgySpkAudio { pcm, .. })) => {
                        queue.push_front(pcm);
                        break;
                    }
                    Err(mpsc::TrySendError::Full(_)) => break,
                    Err(mpsc::TrySendError::Disconnected(_)) => {
                        local_stop.store(true, Ordering::SeqCst);
                        queue.clear();
                        break;
                    }
                }
            }
        };

        while !shutdown.load(Ordering::Relaxed)
            && !local_stop.load(Ordering::Relaxed)
            && !spk_stop_flag.load(Ordering::Relaxed)
        {
            flush_spk_queue(&mut spk_out_queue, &local_stop);

            // Drain SPK_ACK non-blocking so channel stays clean.
            while let Ok((ok, err)) = spk_ack_rx.try_recv() {
                if !ok {
                    crate::log!("voice-codex spk_ack err: {err}");
                }
            }

            // 1. Poll Codex app-server notifications.
            let mut app_server_dead = false;
            loop {
                match sess.ws.poll_json() {
                    Ok(Some(v)) => {
                        let meth = v.get("method").and_then(Value::as_str).unwrap_or("");
                        let params = v.get("params").unwrap_or(&Value::Null);
                        let note_tid = params.get("threadId").and_then(Value::as_str);
                        if note_tid.is_some() && note_tid != Some(sess.thread_id.as_str()) {
                            continue;
                        }
                        match meth {
                            "thread/realtime/closed" | "thread/realtime/error" => {
                                crate::log!("voice-codex remote ended: {meth} {params}");
                                app_server_dead = true;
                                break;
                            }
                            _ => {
                                if streams_opened
                                    && barge_tracker.on_app_server_notification(meth, params)
                                        == BargeInAction::ClearSpeaker
                                {
                                    spk_out_queue.clear();
                                    silence_gate.reset();
                                    spk_dsp.reset();
                                    let _ = ev_tx.send(Event::AgySpkClear {
                                        stream_id: spk_stream_id,
                                    });
                                }
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        crate::log!("voice-codex app-server ws closed: {e}");
                        app_server_dead = true;
                        break;
                    }
                }
            }
            if app_server_dead {
                break;
            }

            let now = Instant::now();
            if !webrtc_connected && now >= connect_deadline {
                crate::log!("voice-codex WebRTC connect timed out");
                break;
            }
            if let Some(since) = disconnected_at {
                if now.duration_since(since) >= WEBRTC_DISCONNECT_GRACE {
                    crate::log!("voice-codex WebRTC disconnected grace exceeded");
                    break;
                }
            }

            // 2. Pace 20 ms Opus mic frames into str0m once WebRTC is connected.
            if webrtc_connected && now >= next_tx_at {
                let mut popped_16k: Option<Vec<i16>> = None;
                // Drain legacy raw MIC_AUDIO chunks if any.
                {
                    let (lock, _) = &*mic_queue;
                    let mut q = lock.lock().unwrap();
                    while let Some(raw) = q.chunks.pop_front() {
                        s16le_bytes_to_i16(&raw, &mut mic_pcm16_scratch);
                        mic_pcm16_pending.extend_from_slice(&mic_pcm16_scratch);
                    }
                }
                if mic_pcm16_pending.len() >= mic_frame_16k {
                    popped_16k = Some(mic_pcm16_pending.drain(..mic_frame_16k).collect());
                } else {
                    let now_ms = crate::serve::now_us() / 1000;
                    let mut guard = mic_opus_jb.lock().unwrap();
                    if let Some(jb) = guard.as_mut() {
                        if let Some(samples) = jb.pop_frame(now_ms) {
                            mic_pcm16_pending.extend_from_slice(&samples);
                            if mic_pcm16_pending.len() >= mic_frame_16k {
                                popped_16k = Some(mic_pcm16_pending.drain(..mic_frame_16k).collect());
                            }
                        }
                    }
                }

                let opus_frame = if let Some(pcm16) = popped_16k {
                    upsampler.process(&pcm16, &mut mic_pcm48);
                    mic_enc.encode(&mic_pcm48)
                } else {
                    mic_enc.encode(&silence_48k)
                };

                if let Ok(encoded) = opus_frame {
                    if let Some(writer) = sess.rtc.writer(sess.audio_mid) {
                        let pt = writer.payload_params().next().map(|p| p.pt());
                        if let Some(pt) = pt {
                            let _ = writer.write(
                                pt,
                                now,
                                MediaTime::new(rtp_ts, Frequency::FORTY_EIGHT_KHZ),
                                encoded,
                            );
                            rtp_ts = rtp_ts.wrapping_add(frame_samples(WEBRTC_SAMPLE_RATE) as u64);
                        }
                    }
                }
                let step = Duration::from_millis(20);
                let next = next_tx_at + step;
                next_tx_at = if now > next + Duration::from_millis(60) {
                    now + step
                } else {
                    next
                };
            }

            // 3. Drive str0m outputs (network transmit + WebRTC events).
            let mut wait_until = now + Duration::from_millis(10);
            loop {
                match sess.rtc.poll_output() {
                    Ok(Output::Timeout(t)) => {
                        wait_until = t.min(now + Duration::from_millis(10));
                        break;
                    }
                    Ok(Output::Transmit(t)) => {
                        let _ = sess.udp.send_to(&t.contents, t.destination);
                    }
                    Ok(Output::Event(ev)) => match ev {
                        RtcEvent::Connected => {
                            crate::log!("voice-codex WebRTC connected (thread={})", sess.thread_id);
                            webrtc_connected = true;
                            disconnected_at = None;
                            next_tx_at = Instant::now();
                            if !streams_opened {
                                let stop_cb = local_stop.clone();
                                let handle = MicStreamHandle::with_closer(
                                    mic_queue.clone(),
                                    mic_opus_jb.clone(),
                                    Box::new(move || stop_cb.store(true, Ordering::SeqCst)),
                                );
                                if ev_tx
                                    .send(Event::AgyMicOpen {
                                        stream_id: mic_stream_id,
                                        handle,
                                    })
                                    .is_err()
                                {
                                    break;
                                }
                                if ev_tx
                                    .send(Event::AgySpkOpen {
                                        stream_id: spk_stream_id,
                                        sample_rate: SPK_SAMPLE_RATE,
                                        ack_tx: spk_ack_tx.clone(),
                                        stop_flag: spk_stop_flag.clone(),
                                    })
                                    .is_err()
                                {
                                    break;
                                }
                                streams_opened = true;
                            }
                        }
                        RtcEvent::IceConnectionStateChange(st) => {
                            use str0m::IceConnectionState;
                            match st {
                                IceConnectionState::Disconnected => {
                                    disconnected_at.get_or_insert_with(Instant::now);
                                }
                                IceConnectionState::Connected | IceConnectionState::Completed => {
                                    disconnected_at = None;
                                }
                                _ => {}
                            }
                        }
                        RtcEvent::ChannelData(cd) => {
                            if let Ok(v) = serde_json::from_slice::<Value>(&cd.data) {
                                if streams_opened
                                    && barge_tracker.on_oai_event(&v) == BargeInAction::ClearSpeaker
                                {
                                    spk_out_queue.clear();
                                    silence_gate.reset();
                                    spk_dsp.reset();
                                    let _ = ev_tx.send(Event::AgySpkClear {
                                        stream_id: spk_stream_id,
                                    });
                                }
                            }
                        }
                        RtcEvent::MediaData(md) => {
                            if !streams_opened {
                                continue;
                            }
                            let mut emit_frame = |opus_len: usize, samples: &[f32]| {
                                if samples.is_empty() {
                                    return;
                                }
                                let decision = silence_gate.accept(opus_len, f32_rms(samples));
                                if decision == GateDecision::Suppress {
                                    return;
                                }
                                if opus_len > SPK_OPUS_SILENCE_MAX_BYTES {
                                    barge_tracker.on_speaker_speech();
                                }
                                spk_dsp.process_to_i16(samples, decision, &mut spk_decoded);
                                let pcm = i16_to_s16le_bytes(&spk_decoded);
                                while spk_out_queue.len() >= SPK_OUT_QUEUE_MAX {
                                    spk_out_queue.pop_front();
                                }
                                spk_out_queue.push_back(pcm);
                                flush_spk_queue(&mut spk_out_queue, &local_stop);
                            };

                            match rtp_gap.on_packet(md.time.numer(), md.audio_start_of_talk_spurt) {
                                RtpGapAction::DropLate => continue,
                                RtpGapAction::Conceal { plc_frames, use_fec } => {
                                    for _ in 0..plc_frames {
                                        if let Ok(n) = spk_dec.decode_plc_float(&mut spk_float) {
                                            if n > 0 {
                                                emit_frame(0, &spk_float);
                                            }
                                        }
                                    }
                                    if use_fec {
                                        if let Ok(n) = spk_dec.decode_float(&md.data, &mut spk_float, true) {
                                            if n > 0 {
                                                emit_frame(0, &spk_float);
                                            }
                                        }
                                    }
                                }
                                RtpGapAction::Normal => {}
                            }

                            if let Ok(n) = spk_dec.decode_float(&md.data, &mut spk_float, false) {
                                if n > 0 {
                                    rtp_gap.advance(md.time.numer(), n);
                                    emit_frame(md.data.len(), &spk_float);
                                }
                            }
                        }
                        _ => {}
                    },
                    Err(e) => {
                        crate::log!("voice-codex str0m poll error: {e}");
                        local_stop.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }

            // 4. Feed incoming UDP datagrams into str0m.
            let mut got_udp = false;
            while let Ok((n, src)) = sess.udp.recv_from(&mut udp_buf) {
                got_udp = true;
                if let Ok(recv) = Receive::new(Protocol::Udp, src, sess.local_addr, &udp_buf[..n]) {
                    let _ = sess.rtc.handle_input(Input::Receive(Instant::now(), recv));
                }
            }
            if !got_udp {
                let now2 = Instant::now();
                let mut sleep_dur = wait_until.saturating_duration_since(now2);
                if webrtc_connected {
                    sleep_dur = sleep_dur.min(next_tx_at.saturating_duration_since(now2));
                }
                let capped = sleep_dur.min(Duration::from_millis(5));
                if !capped.is_zero() {
                    std::thread::sleep(capped);
                }
                let _ = sess.rtc.handle_input(Input::Timeout(Instant::now()));
            }
        }

        // Graceful shutdown: stop Codex realtime session and close VoiceEngine streams.
        let _ = send_stop(&mut sess);
        sess.ws.close();
        if streams_opened {
            let _ = ev_tx.send(Event::AgyMicClosed {
                stream_id: mic_stream_id,
            });
            let _ = ev_tx.send(Event::AgySpkClosed {
                stream_id: spk_stream_id,
                notify_client: true,
            });
        } else {
            let _ = ev_tx.send(Event::Shutdown);
        }
        crate::log!("voice-codex backend exited (thread={})", sess.thread_id);
    }

    fn send_stop(sess: &mut PreparedCodexSession) -> Result<(), String> {
        let stop_id = send_rpc(
            &mut sess.ws,
            &mut sess.rpc_id,
            "thread/realtime/stop",
            json!({ "threadId": sess.thread_id }),
        )?;
        let _ = wait_rpc_resp(&mut sess.ws, stop_id, Duration::from_secs(2));
        Ok(())
    }
}

#[cfg(unix)]
pub(crate) use unix_impl::{prepare, spawn_backend, PreparedCodexSession};

#[cfg(not(unix))]
pub(crate) struct PreparedCodexSession;

#[cfg(not(unix))]
pub(crate) fn prepare(_cfg: &CodexConfig) -> Result<PreparedCodexSession, String> {
    Err("Codex voice backend is currently supported on Linux and macOS hosts only".into())
}

#[cfg(not(unix))]
pub(crate) fn spawn_backend(
    _prepared: PreparedCodexSession,
    _next_stream_id: Arc<AtomicU32>,
    _ev_tx: mpsc::SyncSender<Event>,
    _shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(|| {})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsampler_16k_to_48k_preserves_length_and_continuity() {
        let mut up = Upsampler16To48::new();
        let in1: Vec<i16> = (0..320).map(|i| (i * 30) as i16).collect();
        let mut out1 = Vec::new();
        up.process(&in1, &mut out1);
        assert_eq!(out1.len(), 960);
        // Last sample matches last input sample exactly.
        assert_eq!(*out1.last().unwrap(), *in1.last().unwrap());

        // Continuous ramp into frame 2: first interpolated sample smoothly continues from previous frame's end.
        let in2: Vec<i16> = (320..640).map(|i| (i * 30) as i16).collect();
        let mut out2 = Vec::new();
        up.process(&in2, &mut out2);
        assert_eq!(out2.len(), 960);
        assert!((out2[0] as i32 - *in1.last().unwrap() as i32).abs() <= 15);
    }

    #[test]
    fn barge_in_tracker_fires_clear_only_when_assistant_speaking() {
        let mut t = BargeInTracker::default();
        // User starts speaking while assistant is idle -> no clear.
        assert_eq!(
            t.on_oai_event(&json!({"type": "turn.created", "turn": {"role": "user"}})),
            BargeInAction::None
        );
        // Assistant starts speaking.
        assert_eq!(
            t.on_oai_event(&json!({"type": "turn.created", "turn": {"role": "assistant"}})),
            BargeInAction::None
        );
        // User barges in -> ClearSpeaker!
        assert_eq!(
            t.on_oai_event(&json!({"type": "input_transcript.added", "item": {"text": "Stop"}})),
            BargeInAction::ClearSpeaker
        );
        // Subsequent user transcript delta does NOT double-clear.
        assert_eq!(
            t.on_oai_event(&json!({"type": "input_transcript.added", "item": {"text": " now"}})),
            BargeInAction::None
        );
        // Assistant speaks via RTP then speech_started arrives -> ClearSpeaker.
        t.on_speaker_speech();
        assert_eq!(
            t.on_app_server_notification(
                "thread/realtime/itemAdded",
                &json!({"item": {"type": "input_audio_buffer.speech_started"}})
            ),
            BargeInAction::ClearSpeaker
        );
    }

    #[test]
    fn speaker_silence_gate_forwards_speech_and_tail_then_suppresses_idle() {
        let mut gate = SpeakerSilenceGate::default();

        // Pure initial silence is suppressed.
        assert_eq!(gate.accept(3, 0.0), GateDecision::Suppress);
        // Real speech opens the gate with a click-free fade-in.
        assert_eq!(gate.accept(140, 0.12), GateDecision::FadeIn);
        // Subsequent speech passes cleanly without re-fading.
        assert_eq!(gate.accept(140, 0.12), GateDecision::Pass);
        // Next SPK_TAIL_FRAMES - 1 silent frames pass; final tail frame fades out to 0.
        for _ in 0..(SPK_TAIL_FRAMES - 1) {
            assert_eq!(gate.accept(3, 0.0), GateDecision::Pass);
        }
        assert_eq!(gate.accept(3, 0.0), GateDecision::FadeOut);
        // After hangover expires, idle silence is suppressed again.
        assert_eq!(gate.accept(3, 0.0), GateDecision::Suppress);
        // Next utterance fades in again.
        assert_eq!(gate.accept(140, 0.12), GateDecision::FadeIn);
        // Barge-in reset immediately closes tail.
        gate.reset();
        assert_eq!(gate.accept(3, 0.0), GateDecision::Suppress);
    }

    #[test]
    fn speaker_dsp_soft_limits_overshoots_and_applies_smooth_edge_fades() {
        let mut dsp = SpeakerDsp::default();
        let mut out = Vec::new();
        // Hot +3 dB (1.4x full-scale) signal must NEVER hard-clip to ±32767 and stays below 0.98 ceiling.
        let hot: Vec<f32> = (0..480)
            .map(|i| 1.4 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / SPK_SAMPLE_RATE as f32).sin())
            .collect();
        dsp.process_to_i16(&hot, GateDecision::Pass, &mut out);
        assert_eq!(out.len(), 480);
        let peak = out.iter().map(|&s| (s as i32).abs()).max().unwrap();
        assert!(peak > 26_000 && peak <= 32_200, "unexpected soft-limited peak {peak}");

        // FadeIn starts at exact 0; FadeOut ends at exact 0.
        dsp.reset();
        let tone: Vec<f32> = vec![0.5; 480];
        dsp.process_to_i16(&tone, GateDecision::FadeIn, &mut out);
        assert_eq!(out[0], 0);
        assert!(out[SPK_FADE_SAMPLES] > 5_000);

        dsp.process_to_i16(&tone, GateDecision::FadeOut, &mut out);
        assert_eq!(*out.last().unwrap(), 0);
    }

    #[test]
    fn rtp_gap_tracker_detects_loss_drops_late_and_ignores_talkspurt_pause() {
        let mut tracker = RtpGapTracker::default();
        assert_eq!(tracker.on_packet(10_000, false), RtpGapAction::Normal);
        tracker.advance(10_000, 480); // expected = 10_960

        // Contiguous next 20 ms frame -> Normal.
        assert_eq!(tracker.on_packet(10_960, false), RtpGapAction::Normal);
        tracker.advance(10_960, 480); // expected = 11_920

        // Late / duplicate packet from earlier -> DropLate.
        assert_eq!(tracker.on_packet(10_960, false), RtpGapAction::DropLate);

        // 1 missing 20 ms frame (jump +1920 instead of +960) -> 0 PLC + in-band FEC.
        assert_eq!(
            tracker.on_packet(11_920 + 960, false),
            RtpGapAction::Conceal {
                plc_frames: 0,
                use_fec: true,
            }
        );

        // 3 missing 20 ms frames -> 2 PLC + 1 FEC.
        assert_eq!(
            tracker.on_packet(11_920 + 960 * 3, false),
            RtpGapAction::Conceal {
                plc_frames: 2,
                use_fec: true,
            }
        );

        // Large >250 ms pause or talkspurt marker -> Normal (do not synthesize PLC across silence).
        assert_eq!(tracker.on_packet(11_920 + 48_000, false), RtpGapAction::Normal);
        assert_eq!(tracker.on_packet(11_920 + 960 * 2, true), RtpGapAction::Normal);
    }

    #[test]
    fn opus_decode_float_handles_normal_fec_plc_and_oversized_frames() {
        let mut enc = VoiceOpusEncoder::new(SPK_SAMPLE_RATE, 36_000, false).unwrap();
        let mut dec = VoiceOpusDecoder::new(SPK_SAMPLE_RATE).unwrap();
        let frame: Vec<i16> = (0..480)
            .map(|i| (12_000.0 * (2.0 * std::f32::consts::PI * 300.0 * i as f32 / SPK_SAMPLE_RATE as f32).sin()) as i16)
            .collect();
        let pkt = enc.encode(&frame).unwrap();
        let mut fbuf = Vec::new();
        assert_eq!(dec.decode_float(&pkt, &mut fbuf, false).unwrap(), 480);
        assert_eq!(dec.decode_plc_float(&mut fbuf).unwrap(), 480);
        assert_eq!(dec.decode_float(&pkt, &mut fbuf, true).unwrap(), 480);
    }

    #[cfg(unix)]
    #[test]
    fn unix_ws_client_roundtrip_and_codex_prepare_against_mock_daemon() {
        use base64ct::{Base64, Encoding};
        use sha1::{Digest, Sha1};
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;

        let dir = std::env::temp_dir().join(format!("roamtty-codex-ws-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("app-server-control.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            // 1. Read HTTP Upgrade request & respond 101.
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
                conn.write_all(&frame).unwrap();
            }

            // Build a valid str0m answer peer on the server side to return a real SDP answer!
            let mut ans_rtc = str0m::RtcConfig::new()
                .set_crypto_provider(str0m_rust_crypto::default_provider().into())
                .clear_codecs()
                .enable_opus(true, false)
                .build(Instant::now());
            ans_rtc.add_local_candidate(
                str0m::Candidate::host("127.0.0.1:49999".parse().unwrap(), str0m::net::Protocol::Udp)
                    .unwrap(),
            );

            while let Some(msg) = read_ws(&mut conn) {
                let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
                let id = msg.get("id").and_then(Value::as_u64);
                match method {
                    "initialize" => {
                        write_ws(&mut conn, &json!({"id": id, "result": {"userAgent": "mock"}}));
                    }
                    "initialized" => {}
                    "thread/loaded/list" => {
                        write_ws(&mut conn, &json!({"id": id, "result": {"data": ["thread-mock-1"]}}));
                    }
                    "thread/realtime/start" => {
                        let offer_str = msg["params"]["transport"]["sdp"].as_str().unwrap();
                        assert_eq!(msg["params"]["version"].as_str(), Some("v3"));
                        let offer = str0m::change::SdpOffer::from_sdp_string(offer_str).unwrap();
                        let answer = ans_rtc.sdp_api().accept_offer(offer).unwrap();
                        write_ws(&mut conn, &json!({"id": id, "result": {}}));
                        write_ws(
                            &mut conn,
                            &json!({
                                "method": "thread/realtime/sdp",
                                "params": { "threadId": "thread-mock-1", "sdp": answer.to_sdp_string() }
                            }),
                        );
                        break;
                    }
                    _ => {}
                }
            }
        });

        let cfg = CodexConfig {
            thread_id: Some("thread-mock-1".into()),
            cwd: None,
            sock_path: Some(sock_path.to_string_lossy().into_owned()),
            voice: Some("marin".into()),
        };
        let prepared = prepare(&cfg).expect("prepare against mock codex app-server");
        drop(prepared);
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
