//! Wire format shared by `roamtty-desktop` and the Roamtty apps. See PROTOCOL.md.
//!
//! After the hello line every message is `u8 type | u24 len | payload`, big endian.

pub mod fec;
pub mod udp;

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

/// Protocol version this build speaks. Bumped only when a binary layout or meaning changes.
pub const PROTO: u32 = 1;
pub const MAX_LEN: usize = (1 << 24) - 1;

pub mod ty {
    pub const CONFIG: u8 = 0x01;
    pub const INPUT: u8 = 0x02;
    pub const ACTION: u8 = 0x03;
    pub const REQ: u8 = 0x04;
    pub const ACK: u8 = 0x05;
    pub const CLIP_SET: u8 = 0x06;
    pub const PING: u8 = 0x07;
    pub const PONG: u8 = 0x08;
    pub const VIDEO: u8 = 0x81;
    pub const CURSOR_POS: u8 = 0x82;
    pub const CURSOR_SHAPE: u8 = 0x83;
    pub const STATE: u8 = 0x84;
}

pub mod req {
    pub const KEYFRAME: u8 = 1;
    pub const PAUSE: u8 = 2;
    pub const RESUME: u8 = 3;
    pub const VIEW_ONLY_ON: u8 = 4;
    pub const VIEW_ONLY_OFF: u8 = 5;
    pub const CLIP_GET: u8 = 6;
}

pub mod action {
    pub const SHOW_DESKTOP: u8 = 1;
    pub const ALL_WINDOWS: u8 = 2;
}

pub const VIDEO_KEY: u8 = 1;
pub const VIDEO_REFINE: u8 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Display {
    pub id: u8,
    pub w: u32,
    pub h: u32,
    pub scale: f32,
    pub x: i32,
    pub y: i32,
    pub primary: bool,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Udp {
    pub port: u16,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub proto: u32,
    pub ver: String,
    pub os: String,
    pub session: String,
    pub resume_token: String,
    pub displays: Vec<Display>,
    pub enc: Vec<String>,
    pub hw: String,
    pub needs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub udp: Option<Udp>,
}

/// The first stdout line when `serve` cannot start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartError {
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub min: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub max: Option<u32>,
}

/// `CONFIG` carries only the fields that changed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub display: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub fps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub quality: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub max_bitrate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub udp: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub udp_client_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub lock_on_close: Option<bool>,
    /// Test fixtures only (the plugin honours it with ROAMTTY_DESKTOP_TEST=1): drop this share of UDP datagrams.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub debug_loss: Option<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    Abs { display: u8, x: u16, y: u16 },
    Rel { dx: i16, dy: i16 },
    Button { button: u8, down: bool },
    Wheel { dx: i16, dy: i16 },
    Key { hid: u16, down: bool },
    Text(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientMsg {
    Config(Config),
    Input(Input),
    Action(u8),
    Req(u8),
    Ack { frame: u32, recv_us: u64, decoded_us: u64 },
    ClipSet(String),
    Ping(u64),
    Pong { t1: u64, t2: u64, t3: u64 },
    Unknown(u8),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Video<'a> {
    pub frame: u32,
    pub flags: u8,
    pub capture_us: u64,
    pub encoded_us: u64,
    pub w: u16,
    pub h: u16,
    pub data: &'a [u8],
}

pub fn write_frame(w: &mut impl Write, ty: u8, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_LEN {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "frame too long"));
    }
    let n = payload.len() as u32;
    w.write_all(&[ty, (n >> 16) as u8, (n >> 8) as u8, n as u8])?;
    w.write_all(payload)
}

/// Reads one frame; `Ok(None)` at a clean end of stream.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut head = [0u8; 4];
    match r.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = ((head[1] as usize) << 16) | ((head[2] as usize) << 8) | head[3] as usize;
    let mut payload = vec![0u8; n];
    r.read_exact(&mut payload)?;
    Ok(Some((head[0], payload)))
}

struct Cur<'a>(&'a [u8]);

impl Cur<'_> {
    fn take(&mut self, n: usize) -> io::Result<&[u8]> {
        if self.0.len() < n {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "short payload"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> io::Result<u8> { Ok(self.take(1)?[0]) }
    fn u16(&mut self) -> io::Result<u16> { Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap())) }
    fn i16(&mut self) -> io::Result<i16> { Ok(i16::from_be_bytes(self.take(2)?.try_into().unwrap())) }
    fn u32(&mut self) -> io::Result<u32> { Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap())) }
    fn u64(&mut self) -> io::Result<u64> { Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap())) }
}

fn text(b: &[u8]) -> io::Result<String> {
    String::from_utf8(b.to_vec()).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad utf-8"))
}

fn json<T: for<'de> Deserialize<'de>>(b: &[u8]) -> io::Result<T> {
    serde_json::from_slice(b).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn decode_client(ty: u8, p: &[u8]) -> io::Result<ClientMsg> {
    let mut c = Cur(p);
    Ok(match ty {
        ty::CONFIG => ClientMsg::Config(json(p)?),
        ty::INPUT => ClientMsg::Input(match c.u8()? {
            1 => Input::Abs { display: c.u8()?, x: c.u16()?, y: c.u16()? },
            2 => Input::Rel { dx: c.i16()?, dy: c.i16()? },
            3 => Input::Button { button: c.u8()?, down: c.u8()? != 0 },
            4 => Input::Wheel { dx: c.i16()?, dy: c.i16()? },
            5 => Input::Key { hid: c.u16()?, down: c.u8()? != 0 },
            6 => Input::Text(text(c.0)?),
            k => return Err(io::Error::new(io::ErrorKind::InvalidData, format!("input kind {k}"))),
        }),
        ty::ACTION => ClientMsg::Action(c.u8()?),
        ty::REQ => ClientMsg::Req(c.u8()?),
        ty::ACK => ClientMsg::Ack { frame: c.u32()?, recv_us: c.u64()?, decoded_us: c.u64()? },
        ty::CLIP_SET => ClientMsg::ClipSet(text(p)?),
        ty::PING => ClientMsg::Ping(c.u64()?),
        ty::PONG => ClientMsg::Pong { t1: c.u64()?, t2: c.u64()?, t3: c.u64()? },
        other => ClientMsg::Unknown(other),
    })
}

/// Encodes a client message; the app has its own encoder, this one feeds tests and golden files.
pub fn encode_client(m: &ClientMsg) -> (u8, Vec<u8>) {
    let mut v = Vec::new();
    let t = match m {
        ClientMsg::Config(c) => { v = serde_json::to_vec(c).unwrap(); ty::CONFIG }
        ClientMsg::Input(i) => {
            match i {
                Input::Abs { display, x, y } => { v.push(1); v.push(*display); v.extend(x.to_be_bytes()); v.extend(y.to_be_bytes()); }
                Input::Rel { dx, dy } => { v.push(2); v.extend(dx.to_be_bytes()); v.extend(dy.to_be_bytes()); }
                Input::Button { button, down } => { v.push(3); v.push(*button); v.push(*down as u8); }
                Input::Wheel { dx, dy } => { v.push(4); v.extend(dx.to_be_bytes()); v.extend(dy.to_be_bytes()); }
                Input::Key { hid, down } => { v.push(5); v.extend(hid.to_be_bytes()); v.push(*down as u8); }
                Input::Text(s) => { v.push(6); v.extend(s.as_bytes()); }
            }
            ty::INPUT
        }
        ClientMsg::Action(a) => { v.push(*a); ty::ACTION }
        ClientMsg::Req(r) => { v.push(*r); ty::REQ }
        ClientMsg::Ack { frame, recv_us, decoded_us } => {
            v.extend(frame.to_be_bytes()); v.extend(recv_us.to_be_bytes()); v.extend(decoded_us.to_be_bytes()); ty::ACK
        }
        ClientMsg::ClipSet(s) => { v.extend(s.as_bytes()); ty::CLIP_SET }
        ClientMsg::Ping(t1) => { v.extend(t1.to_be_bytes()); ty::PING }
        ClientMsg::Pong { t1, t2, t3 } => { v.extend(t1.to_be_bytes()); v.extend(t2.to_be_bytes()); v.extend(t3.to_be_bytes()); ty::PONG }
        ClientMsg::Unknown(t) => *t,
    };
    (t, v)
}

pub fn video_payload(v: &Video) -> Vec<u8> {
    let mut p = Vec::with_capacity(25 + v.data.len());
    p.extend(v.frame.to_be_bytes());
    p.push(v.flags);
    p.extend(v.capture_us.to_be_bytes());
    p.extend(v.encoded_us.to_be_bytes());
    p.extend(v.w.to_be_bytes());
    p.extend(v.h.to_be_bytes());
    p.extend_from_slice(v.data);
    p
}

pub fn cursor_pos_payload(x: u16, y: u16, visible: bool, shape_hash: u64) -> Vec<u8> {
    let mut p = Vec::with_capacity(13);
    p.extend(x.to_be_bytes());
    p.extend(y.to_be_bytes());
    p.push(visible as u8);
    p.extend(shape_hash.to_be_bytes());
    p
}

pub fn cursor_shape_payload(hash: u64, w: u16, h: u16, hot_x: u16, hot_y: u16, rgba: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(16 + rgba.len());
    p.extend(hash.to_be_bytes());
    for n in [w, h, hot_x, hot_y] {
        p.extend(n.to_be_bytes());
    }
    p.extend_from_slice(rgba);
    p
}

pub fn pong_payload(t1: u64, t2: u64, t3: u64) -> Vec<u8> {
    let mut p = Vec::with_capacity(24);
    for n in [t1, t2, t3] {
        p.extend(n.to_be_bytes());
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn testdata() -> PathBuf { PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata") }

    /// Every sample the apps check their own codec against: name, frame type, payload.
    fn samples() -> Vec<(&'static str, u8, Vec<u8>)> {
        let mut s: Vec<(&'static str, u8, Vec<u8>)> = [
            ("config", ClientMsg::Config(Config { fps: Some(60), quality: Some("clear".into()), ..Default::default() })),
            ("input-abs", ClientMsg::Input(Input::Abs { display: 0, x: 1512, y: 982 })),
            ("input-rel", ClientMsg::Input(Input::Rel { dx: -5, dy: 12 })),
            ("input-button", ClientMsg::Input(Input::Button { button: 2, down: true })),
            ("input-wheel", ClientMsg::Input(Input::Wheel { dx: 0, dy: -240 })),
            ("input-key", ClientMsg::Input(Input::Key { hid: 0xE3, down: true })),
            ("input-text", ClientMsg::Input(Input::Text("你好 ok".into()))),
            ("action", ClientMsg::Action(action::ALL_WINDOWS)),
            ("req", ClientMsg::Req(req::KEYFRAME)),
            ("ack", ClientMsg::Ack { frame: 7, recv_us: 1_000_001, decoded_us: 1_004_002 }),
            ("clip-set", ClientMsg::ClipSet("copied".into())),
            ("ping", ClientMsg::Ping(123_456_789)),
            ("pong", ClientMsg::Pong { t1: 1, t2: 2, t3: 3 }),
        ]
        .into_iter()
        .map(|(n, m)| { let (t, p) = encode_client(&m); (n, t, p) })
        .collect();
        s.push(("video", ty::VIDEO, video_payload(&Video {
            frame: 42, flags: VIDEO_KEY, capture_us: 10, encoded_us: 20, w: 1920, h: 1080, data: &[0, 0, 0, 1, 0x67, 0x42],
        })));
        s.push(("cursor-pos", ty::CURSOR_POS, cursor_pos_payload(300, 200, true, 0xDEAD_BEEF)));
        s.push(("cursor-shape", ty::CURSOR_SHAPE, cursor_shape_payload(0xDEAD_BEEF, 2, 1, 1, 0, &[255, 0, 0, 255, 0, 255, 0, 128])));
        s.push(("state", ty::STATE, br#"{"kicked":true}"#.to_vec()));
        s
    }

    fn hello() -> Hello {
        Hello {
            proto: 1, ver: "0.1.0".into(), os: "linux".into(), session: "9f2c".into(), resume_token: "AAAAAAAAAAAAAAAAAAAAAA==".into(),
            displays: vec![Display { id: 0, w: 1920, h: 1080, scale: 1.0, x: 0, y: 0, primary: true, name: "screen".into() }],
            enc: vec!["h264".into()], hw: "openh264".into(), needs: vec![], udp: None,
        }
    }

    /// `UPDATE_GOLDEN=1 cargo test` rewrites testdata/; otherwise the files must match.
    #[test]
    fn golden_samples() {
        let dir = testdata();
        let update = std::env::var_os("UPDATE_GOLDEN").is_some();
        for (name, t, p) in samples() {
            let mut bytes = Vec::new();
            write_frame(&mut bytes, t, &p).unwrap();
            let path = dir.join(format!("{name}.bin"));
            if update { std::fs::write(&path, &bytes).unwrap(); }
            assert_eq!(std::fs::read(&path).unwrap(), bytes, "{name}");
            let (rt, rp) = read_frame(&mut bytes.as_slice()).unwrap().unwrap();
            assert_eq!((rt, rp.as_slice()), (t, p.as_slice()));
            if t < 0x80 {
                let m = decode_client(t, &p).unwrap();
                assert_eq!(encode_client(&m), (t, p.clone()), "{name} round trip");
            }
        }
        // UDP: one sealed heartbeat and the fragments of a 3000-byte video message at 50% parity,
        // each fragment as `u16 len | plaintext`.
        let mut key = [0u8; 16];
        for (i, b) in key.iter_mut().enumerate() { *b = i as u8; }
        let sealed = udp::Crypto::new(&key).seal(udp::DIR_S2C, 7, &udp::Packet::Heartbeat { t: 1, echo: 2 }.encode());
        let message: Vec<u8> = (0..3000u32).map(|i| (i * 13 % 256) as u8).collect();
        let mut frags = Vec::new();
        for f in udp::fragment(5, udp::FLAG_VIDEO, &message, 0.5) {
            let p = udp::Packet::Frag(f).encode();
            frags.extend((p.len() as u16).to_be_bytes());
            frags.extend(p);
        }
        for (name, bytes) in [("udp-sealed.bin", sealed), ("udp-frags.bin", frags)] {
            let path = dir.join(name);
            if update { std::fs::write(&path, &bytes).unwrap(); }
            assert_eq!(std::fs::read(&path).unwrap(), bytes, "{name}");
        }
        let json = serde_json::to_string(&hello()).unwrap();
        let path = dir.join("hello.json");
        if update { std::fs::write(&path, format!("{json}\n")).unwrap(); }
        let stored: Hello = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(stored, hello());
    }

    #[test]
    fn unknown_types_are_kept_as_unknown() {
        assert_eq!(decode_client(0x55, &[1, 2]).unwrap(), ClientMsg::Unknown(0x55));
    }

    #[test]
    fn end_of_stream_is_none() {
        assert!(read_frame(&mut [].as_slice()).unwrap().is_none());
    }
}
