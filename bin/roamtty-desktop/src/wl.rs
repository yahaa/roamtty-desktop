//! Linux Wayland backend for wlroots compositors (sway, Hyprland, labwc, river…): frames from
//! wlr-screencopy (cursor drawn in), input through wlr-virtual-pointer and zwp-virtual-keyboard,
//! clipboard through wlr-data-control. GNOME and KDE need the desktop portal instead.

use crate::backend::{Backend, Cursor, Frame, Result};
use proto::Display;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use wayland_client::protocol::{wl_output, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_buffer};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum, Proxy};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{zwp_virtual_keyboard_manager_v1, zwp_virtual_keyboard_v1};
use wayland_protocols_wlr::data_control::v1::client::{zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_offer_v1, zwlr_data_control_source_v1};
use wayland_protocols_wlr::screencopy::v1::client::{zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1};
use wayland_protocols_wlr::virtual_pointer::v1::client::{zwlr_virtual_pointer_manager_v1, zwlr_virtual_pointer_v1};

fn err(e: impl std::fmt::Display) -> String { e.to_string() }

/// The user's Wayland socket: WAYLAND_DISPLAY / XDG_RUNTIME_DIR from the environment, from the
/// user's own processes, or the first `wayland-*` socket in /run/user/<uid>.
pub fn find_socket() -> Option<(PathBuf, String)> {
    let uid = unsafe { libc::getuid() };
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(format!("/run/user/{uid}")));
    if let Ok(name) = std::env::var("WAYLAND_DISPLAY") {
        if runtime.join(&name).exists() { return Some((runtime, name)); }
    }
    if let Ok(dir) = std::fs::read_dir("/proc") {
        for e in dir.flatten() {
            if !e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()) { continue; }
            use std::os::unix::fs::MetadataExt;
            if std::fs::metadata(e.path()).map(|m| m.uid()).ok() != Some(uid) { continue; }
            let Ok(env) = std::fs::read(e.path().join("environ")) else { continue };
            let (mut wd, mut rd) = (None, None);
            for kv in env.split(|b| *b == 0) {
                if let Some(v) = kv.strip_prefix(b"WAYLAND_DISPLAY=") { wd = Some(String::from_utf8_lossy(v).into_owned()); }
                if let Some(v) = kv.strip_prefix(b"XDG_RUNTIME_DIR=") { rd = Some(PathBuf::from(String::from_utf8_lossy(v).into_owned())); }
            }
            match (wd, rd) {
                (Some(w), Some(r)) if r.join(&w).exists() => return Some((r, w)),
                // A compositor itself has the runtime dir but not WAYLAND_DISPLAY: look for its socket.
                (None, Some(r)) => if let Some(n) = socket_in(&r, uid) { return Some((r, n)); },
                _ => {}
            }
        }
    }
    socket_in(&runtime, uid).map(|n| (runtime, n))
}

/// The first `wayland-N` socket in `dir` owned by `uid`.
fn socket_in(dir: &std::path::Path, uid: u32) -> Option<String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let mut names: Vec<String> = std::fs::read_dir(dir).ok()?.flatten()
        .filter(|e| e.metadata().is_ok_and(|m| m.file_type().is_socket() && m.uid() == uid))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("wayland-")).collect();
    names.sort();
    names.into_iter().next()
}

#[derive(Default, Clone)]
struct Output { x: i32, y: i32, w: i32, h: i32, name: String, done: bool }

#[derive(Default)]
struct CopyState { format: Option<wl_shm::Format>, w: u32, h: u32, stride: u32, ready: bool, failed: bool, y_invert: bool }

#[derive(Default)]
struct State {
    outputs: Vec<(wl_output::WlOutput, Output)>,
    shm: Option<wl_shm::WlShm>,
    seat: Option<wl_seat::WlSeat>,
    screencopy: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    vpointer: Option<zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1>,
    vkeyboard: Option<zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1>,
    data_control: Option<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1>,
    copy: CopyState,
    /// The offer of the current selection and its MIME types.
    offers: HashMap<u32, Vec<String>>,
    selection: Option<zwlr_data_control_offer_v1::ZwlrDataControlOfferV1>,
    /// Text we own as the selection; served on request.
    owned: Option<String>,
}

pub struct Wayland {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    pointer: zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    keyboard: zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    device: Option<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1>,
    source: Option<zwlr_data_control_source_v1::ZwlrDataControlSourceV1>,
    shm_buf: Option<ShmBuffer>,
    start: std::time::Instant,
    /// The full extent of all outputs, for absolute pointer motion.
    extent: (i32, i32, i32, i32),
    wheel: (i32, i32),
    /// Depressed modifiers as XKB masks: a virtual keyboard reports them itself.
    mods: u32,
}

struct ShmBuffer { ptr: *mut u8, size: usize, buffer: wl_buffer::WlBuffer, pool: wl_shm_pool::WlShmPool, _fd: OwnedFd, w: u32, h: u32, stride: u32 }

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.size); }
    }
}

/// A fully resolved US keymap (`xkbcli compile-keymap --rules evdev --model pc105 --layout us`), so
/// nothing depends on the host's XKB include paths. Spare keys are spliced into it per text.
const BASE_KEYMAP: &str = include_str!("us.xkb");

/// Evdev codes borrowed for characters the US layout lacks (beyond any real key).
const SPARE_BASE: u32 = 712;
const SPARE: usize = 16;

fn keymap_text(extra: &[u32]) -> String {
    if extra.is_empty() { return BASE_KEYMAP.to_string(); }
    let codes: String = extra.iter().enumerate().map(|(i, _)| format!("\t<RT{i:02}> = {};\n", SPARE_BASE + i as u32 + 8)).collect();
    let syms: String = extra.iter().enumerate().map(|(i, ks)| format!("\tkey <RT{i:02}> {{ [ 0x{ks:08x} ] }};\n")).collect();
    let mut k = BASE_KEYMAP.replacen("maximum = 708;", "maximum = 760;", 1);
    // The keycodes section is the first block; the symbols section ends just before the keymap's own close.
    let codes_end = k.find("\n};\n").expect("keycodes section");
    k.insert_str(codes_end + 1, &codes);
    let syms_start = k.find("xkb_symbols").expect("symbols section");
    let syms_end = syms_start + k[syms_start..].find("\n};\n").expect("symbols end");
    k.insert_str(syms_end + 1, &syms);
    k
}

fn memfd(name: &str, data: &[u8]) -> Result<OwnedFd> {
    let cname = std::ffi::CString::new(name).unwrap();
    let fd = unsafe { libc::memfd_create(cname.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 { return Err("memfd_create failed".into()); }
    let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
    f.write_all(data).map_err(err)?;
    Ok(f.into())
}

/// The US-layout key for an ASCII character: (evdev code, shift).
fn us_key(c: char) -> Option<(u32, bool)> {
    const ROW: &[(char, char, u32)] = &[
        ('1','!',2),('2','@',3),('3','#',4),('4','$',5),('5','%',6),('6','^',7),('7','&',8),('8','*',9),('9','(',10),('0',')',11),
        ('-','_',12),('=','+',13),('[','{',26),(']','}',27),(';',':',39),('\'','"',40),('`','~',41),('\\','|',43),(',','<',51),('.','>',52),('/','?',53),
    ];
    const LETTERS: &str = "qwertyuiopasdfghjklzxcvbnm";
    const CODES: [u32; 26] = [16,17,18,19,20,21,22,23,24,25,30,31,32,33,34,35,36,37,38,44,45,46,47,48,49,50];
    if c == ' ' { return Some((57, false)); }
    if c == '\n' || c == '\r' { return Some((28, false)); }
    if c == '\t' { return Some((15, false)); }
    if let Some(i) = LETTERS.find(c.to_ascii_lowercase()) { if c.is_ascii_alphabetic() { return Some((CODES[i], c.is_ascii_uppercase())); } }
    ROW.iter().find_map(|(a, b, k)| if *a == c { Some((*k, false)) } else if *b == c { Some((*k, true)) } else { None })
}

impl Wayland {
    pub fn open() -> Result<Wayland> {
        let (runtime, name) = find_socket().ok_or("no Wayland display for this user")?;
        std::env::set_var("XDG_RUNTIME_DIR", &runtime);
        std::env::set_var("WAYLAND_DISPLAY", &name);
        let conn = Connection::connect_to_env().map_err(err)?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut state = State::default();
        queue.roundtrip(&mut state).map_err(err)?;
        queue.roundtrip(&mut state).map_err(err)?;
        let missing: Vec<&str> = [("wlr-screencopy", state.screencopy.is_none()), ("wl_shm", state.shm.is_none()),
            ("wlr-virtual-pointer", state.vpointer.is_none()), ("virtual-keyboard", state.vkeyboard.is_none())]
            .iter().filter(|(_, m)| *m).map(|(n, _)| *n).collect();
        if !missing.is_empty() {
            return Err(format!("compositor lacks {} (GNOME and KDE use the desktop portal, not supported yet)", missing.join(", ")));
        }
        let pointer = state.vpointer.as_ref().unwrap().create_virtual_pointer(state.seat.as_ref(), &qh, ());
        let seat = state.seat.clone().ok_or("no wl_seat")?;
        let keyboard = state.vkeyboard.as_ref().unwrap().create_virtual_keyboard(&seat, &qh, ());
        let device = state.data_control.as_ref().map(|m| m.get_data_device(&seat, &qh, ()));
        let mut w = Wayland { conn, queue, state, pointer, keyboard, device, source: None, shm_buf: None,
            start: std::time::Instant::now(), extent: (0, 0, 1, 1), wheel: (0, 0), mods: 0 };
        w.upload_keymap(&[])?;
        let ds = w.displays();
        let (x0, y0) = (ds.iter().map(|d| d.x).min().unwrap_or(0), ds.iter().map(|d| d.y).min().unwrap_or(0));
        let x1 = ds.iter().map(|d| d.x + d.w as i32).max().unwrap_or(1);
        let y1 = ds.iter().map(|d| d.y + d.h as i32).max().unwrap_or(1);
        w.extent = (x0, y0, x1 - x0, y1 - y0);
        w.queue.roundtrip(&mut w.state).map_err(err)?;
        Ok(w)
    }

    fn time(&self) -> u32 { self.start.elapsed().as_millis() as u32 }

    fn upload_keymap(&mut self, extra: &[u32]) -> Result<()> {
        let mut text = keymap_text(extra).into_bytes();
        text.push(0);
        let fd = memfd("roamtty-keymap", &text)?;
        self.keyboard.keymap(1 /* xkb v1 */, fd.as_fd(), text.len() as u32);
        self.conn.flush().map_err(err)
    }

    fn key_code(&mut self, code: u32, down: bool) {
        let t = self.time();
        self.keyboard.key(t, code, down as u32);
        // Shift, Control, Alt (Mod1), Super (Mod4) in the standard XKB modifier order.
        let mask = match code { 42 | 54 => 1, 29 | 97 => 4, 56 | 100 => 8, 125 | 126 => 64, _ => 0 };
        if mask != 0 {
            if down { self.mods |= mask } else { self.mods &= !mask }
            self.keyboard.modifiers(self.mods, 0, 0, 0);
        }
    }

    fn dispatch(&mut self) { let _ = self.queue.dispatch_pending(&mut self.state); let _ = self.conn.flush(); }

    fn buffer_for(&mut self, w: u32, h: u32, stride: u32, format: wl_shm::Format) -> Result<()> {
        if self.shm_buf.as_ref().is_some_and(|b| b.w == w && b.h == h && b.stride == stride) { return Ok(()); }
        self.shm_buf = None;
        let size = (stride * h) as usize;
        let fd = memfd("roamtty-frame", &[])?;
        if unsafe { libc::ftruncate(std::os::fd::AsRawFd::as_raw_fd(&fd), size as i64) } != 0 { return Err("ftruncate failed".into()); }
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, std::os::fd::AsRawFd::as_raw_fd(&fd), 0) };
        if ptr == libc::MAP_FAILED { return Err("mmap failed".into()); }
        let qh = self.queue.handle();
        let pool = self.state.shm.as_ref().unwrap().create_pool(fd.as_fd(), size as i32, &qh, ());
        let buffer = pool.create_buffer(0, w as i32, h as i32, stride as i32, format, &qh, ());
        self.shm_buf = Some(ShmBuffer { ptr: ptr as *mut u8, size, buffer, pool, _fd: fd, w, h, stride });
        Ok(())
    }
}

impl Backend for Wayland {
    fn kind(&self) -> &'static str { "wlroots" }

    fn displays(&self) -> Vec<Display> {
        let mut out: Vec<Display> = self.state.outputs.iter().enumerate().map(|(i, (_, o))| Display {
            id: i as u8, w: o.w.max(1) as u32, h: o.h.max(1) as u32, scale: 1.0, x: o.x, y: o.y, primary: i == 0,
            name: if o.name.is_empty() { format!("output {i}") } else { o.name.clone() },
        }).collect();
        if out.is_empty() { out.push(Display { id: 0, w: 1, h: 1, scale: 1.0, x: 0, y: 0, primary: true, name: "none".into() }); }
        out
    }

    fn capture<'a>(&'a mut self, d: &Display, buf: &'a mut Vec<u8>) -> Result<Frame<'a>> {
        let output = self.state.outputs.get(d.id as usize).map(|(o, _)| o.clone()).ok_or("no such output")?;
        let qh = self.queue.handle();
        self.state.copy = CopyState::default();
        // The cursor is drawn into the frame: wlroots tells clients neither its position nor its shape.
        let frame = self.state.screencopy.as_ref().unwrap().capture_output(1, &output, &qh, ());
        while self.state.copy.format.is_none() && !self.state.copy.failed {
            self.queue.blocking_dispatch(&mut self.state).map_err(err)?;
        }
        if self.state.copy.failed { frame.destroy(); return Err("screencopy failed".into()); }
        let (w, h, stride, format) = (self.state.copy.w, self.state.copy.h, self.state.copy.stride, self.state.copy.format.unwrap());
        self.buffer_for(w, h, stride, format)?;
        frame.copy(&self.shm_buf.as_ref().unwrap().buffer);
        while !self.state.copy.ready && !self.state.copy.failed {
            self.queue.blocking_dispatch(&mut self.state).map_err(err)?;
        }
        frame.destroy();
        if self.state.copy.failed { return Err("screencopy copy failed".into()); }
        let b = self.shm_buf.as_ref().unwrap();
        let data = unsafe { std::slice::from_raw_parts(b.ptr, b.size) };
        // XRGB8888 / ARGB8888 little-endian is B, G, R, X in memory, as X11 gives it.
        if self.state.copy.y_invert {
            buf.resize(data.len(), 0);
            for row in 0..h as usize {
                let s = stride as usize;
                buf[row * s..(row + 1) * s].copy_from_slice(&data[(h as usize - 1 - row) * s..(h as usize - row) * s]);
            }
            return Ok(Frame { bgra: buf, w: w as usize, h: h as usize, stride: stride as usize });
        }
        Ok(Frame { bgra: data, w: w as usize, h: h as usize, stride: stride as usize })
    }

    fn cursor(&mut self) -> Option<Cursor> { None }

    fn move_abs(&mut self, x: i32, y: i32) -> Result<()> {
        let (x0, y0, w, h) = self.extent;
        let t = self.time();
        self.pointer.motion_absolute(t, (x - x0).clamp(0, w - 1) as u32, (y - y0).clamp(0, h - 1) as u32, w as u32, h as u32);
        self.pointer.frame();
        Ok(())
    }

    fn move_rel(&mut self, dx: i16, dy: i16) -> Result<()> {
        let t = self.time();
        self.pointer.motion(t, dx as f64, dy as f64);
        self.pointer.frame();
        Ok(())
    }

    fn button(&mut self, button: u8, down: bool) -> Result<()> {
        let code = match button { 1 => 0x110, 2 => 0x111, 3 => 0x112, _ => return Ok(()) };
        let t = self.time();
        use wayland_client::protocol::wl_pointer::ButtonState;
        self.pointer.button(t, code, if down { ButtonState::Pressed } else { ButtonState::Released });
        self.pointer.frame();
        Ok(())
    }

    fn wheel(&mut self, dx: i16, dy: i16) -> Result<()> {
        use wayland_client::protocol::wl_pointer::{Axis, AxisSource};
        self.wheel.0 += dx as i32;
        self.wheel.1 += dy as i32;
        let t = self.time();
        let (vx, vy) = (self.wheel.0 / 120, self.wheel.1 / 120);
        if vx == 0 && vy == 0 { return Ok(()); }
        self.wheel.0 -= vx * 120;
        self.wheel.1 -= vy * 120;
        self.pointer.axis_source(AxisSource::Wheel);
        // Positive dy scrolls up; Wayland's vertical axis grows downwards.
        if vy != 0 { self.pointer.axis_discrete(t, Axis::VerticalScroll, -15.0 * vy as f64, -vy); }
        if vx != 0 { self.pointer.axis_discrete(t, Axis::HorizontalScroll, 15.0 * vx as f64, vx); }
        self.pointer.frame();
        Ok(())
    }

    fn key(&mut self, hid: u16, down: bool) -> Result<()> {
        if let Some(code) = crate::keymap::hid_to_evdev(hid) { self.key_code(code as u32, down); }
        Ok(())
    }

    /// ASCII through the US keys; anything else through spare keycodes mapped to the characters in
    /// a keymap uploaded for this text, then the base keymap again.
    fn text(&mut self, s: &str) -> Result<()> {
        let chars: Vec<char> = s.chars().collect();
        for chunk in chars.chunks(SPARE) {
            let extra: Vec<u32> = chunk.iter().filter(|c| us_key(**c).is_none()).map(|c| 0x0100_0000 | *c as u32).collect::<Vec<_>>();
            let mut unique: Vec<u32> = Vec::new();
            for k in extra { if !unique.contains(&k) { unique.push(k); } }
            if !unique.is_empty() {
                self.upload_keymap(&unique)?;
                self.queue.roundtrip(&mut self.state).map_err(err)?;
            }
            for c in chunk {
                if let Some((code, shift)) = us_key(*c) {
                    if shift { self.key_code(42, true); }
                    self.key_code(code, true);
                    self.key_code(code, false);
                    if shift { self.key_code(42, false); }
                } else {
                    let i = unique.iter().position(|k| *k == (0x0100_0000 | *c as u32)).unwrap() as u32;
                    self.key_code(SPARE_BASE + i, true);
                    self.key_code(SPARE_BASE + i, false);
                }
            }
            if !unique.is_empty() {
                self.queue.roundtrip(&mut self.state).map_err(err)?;
                // Clients read the keys with the keymap they arrived under before it changes again.
                std::thread::sleep(std::time::Duration::from_millis(30));
                self.upload_keymap(&[])?;
            }
        }
        self.flush();
        Ok(())
    }

    fn tap(&mut self, hids: &[u16]) -> Result<()> {
        for h in hids { self.key(*h, true)?; }
        for h in hids.iter().rev() { self.key(*h, false)?; }
        self.flush();
        Ok(())
    }

    fn flush(&mut self) { self.dispatch(); }

    fn clip_set(&mut self, text: String) {
        let (Some(mgr), Some(device)) = (self.state.data_control.as_ref(), self.device.as_ref()) else { return };
        let qh = self.queue.handle();
        let source = mgr.create_data_source(&qh, ());
        for m in ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "STRING", "TEXT"] { source.offer(m.into()); }
        device.set_selection(Some(&source));
        self.state.owned = Some(text);
        self.source = Some(source);
        self.dispatch();
    }

    fn clip_get(&mut self) -> Option<String> {
        if let Some(t) = &self.state.owned { return Some(t.clone()); }
        let _ = self.queue.roundtrip(&mut self.state);
        let offer = self.state.selection.clone()?;
        let mimes = self.state.offers.get(&offer.id().protocol_id()).cloned().unwrap_or_default();
        let mime = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "STRING", "TEXT"].iter()
            .find(|m| mimes.iter().any(|x| x == *m))?.to_string();
        let mut fds = [0; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 { return None; }
        let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        offer.receive(mime, w.as_fd());
        let _ = self.conn.flush();
        drop(w);
        let mut f = std::fs::File::from(r);
        let mut out = Vec::new();
        // The owner writes and closes; a dead owner is bounded by the client's own timeout.
        let _ = f.read_to_end(&mut out);
        Some(String::from_utf8_lossy(&out).into_owned())
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(s: &mut Self, r: &wl_registry::WlRegistry, e: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = e {
            match interface.as_str() {
                "wl_output" => { let o = r.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, ()); s.outputs.push((o, Output::default())); }
                "wl_shm" => s.shm = Some(r.bind(name, 1, qh, ())),
                "wl_seat" => if s.seat.is_none() { s.seat = Some(r.bind(name, version.min(5), qh, ())); },
                "zwlr_screencopy_manager_v1" => s.screencopy = Some(r.bind(name, version.min(3), qh, ())),
                "zwlr_virtual_pointer_manager_v1" => s.vpointer = Some(r.bind(name, version.min(2), qh, ())),
                "zwp_virtual_keyboard_manager_v1" => s.vkeyboard = Some(r.bind(name, 1, qh, ())),
                "zwlr_data_control_manager_v1" => s.data_control = Some(r.bind(name, version.min(2), qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(s: &mut Self, o: &wl_output::WlOutput, e: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let Some((_, out)) = s.outputs.iter_mut().find(|(x, _)| x == o) else { return };
        match e {
            wl_output::Event::Geometry { x, y, .. } => { out.x = x; out.y = y; }
            wl_output::Event::Mode { flags, width, height, .. } => {
                if matches!(flags, WEnum::Value(f) if f.contains(wl_output::Mode::Current)) { out.w = width; out.h = height; }
            }
            wl_output::Event::Name { name } => out.name = name,
            wl_output::Event::Done => out.done = true,
            _ => {}
        }
    }
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for State {
    fn event(s: &mut Self, _: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, e: zwlr_screencopy_frame_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        use zwlr_screencopy_frame_v1::Event;
        match e {
            Event::Buffer { format: WEnum::Value(f), width, height, stride } => {
                // Only the 32-bit formats laid out like X11's frames; the compositor offers one of them.
                if matches!(f, wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888) {
                    s.copy.format = Some(f); s.copy.w = width; s.copy.h = height; s.copy.stride = stride;
                }
            }
            Event::Flags { flags: WEnum::Value(f) } => s.copy.y_invert = f.contains(zwlr_screencopy_frame_v1::Flags::YInvert),
            Event::Ready { .. } => s.copy.ready = true,
            Event::Failed => s.copy.failed = true,
            Event::BufferDone => { if s.copy.format.is_none() { s.copy.failed = true; } }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, ()> for State {
    fn event(s: &mut Self, _: &zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, e: zwlr_data_control_device_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        use zwlr_data_control_device_v1::Event;
        match e {
            Event::DataOffer { id } => { s.offers.insert(id.id().protocol_id(), Vec::new()); }
            Event::Selection { id } => {
                if let Some(old) = s.selection.take() { if Some(&old) != id.as_ref() { s.offers.remove(&old.id().protocol_id()); old.destroy(); } }
                s.selection = id;
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()> for State {
    fn event(s: &mut Self, o: &zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, e: zwlr_data_control_offer_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = e {
            s.offers.entry(o.id().protocol_id()).or_default().push(mime_type);
        }
    }
}

impl Dispatch<zwlr_data_control_source_v1::ZwlrDataControlSourceV1, ()> for State {
    fn event(s: &mut Self, src: &zwlr_data_control_source_v1::ZwlrDataControlSourceV1, e: zwlr_data_control_source_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match e {
            zwlr_data_control_source_v1::Event::Send { fd, .. } => {
                if let Some(t) = &s.owned { let mut f = std::fs::File::from(fd); let _ = f.write_all(t.as_bytes()); }
            }
            zwlr_data_control_source_v1::Event::Cancelled => { s.owned = None; src.destroy(); }
            _ => {}
        }
    }
}

macro_rules! ignore {
    ($($t:ty),*) => { $(impl Dispatch<$t, ()> for State {
        fn event(_: &mut Self, _: &$t, _: <$t as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    })* };
}
ignore!(wl_shm::WlShm, wl_seat::WlSeat, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer,
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1, zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1, zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1, zwlr_data_control_manager_v1::ZwlrDataControlManagerV1);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keymap_has_spares() {
        let k = keymap_text(&[0x0100_4f60, 0x0100_597d]);
        assert!(k.contains("<RT00> = 720;"));
        assert!(k.contains("key <RT01> { [ 0x0100597d ] };"));
        assert!(k.contains("maximum = 760;"));
        let codes = k.find("<RT00> = 720;").unwrap();
        assert!(codes < k.find("xkb_types").unwrap(), "spares are inside the keycodes section");
        assert!(k.find("key <RT00>").unwrap() > k.find("xkb_symbols").unwrap());
        assert!(!keymap_text(&[]).contains("RT00"));
    }

    #[test]
    fn us_keys() {
        assert_eq!(us_key('a'), Some((30, false)));
        assert_eq!(us_key('A'), Some((30, true)));
        assert_eq!(us_key('>'), Some((52, true)));
        assert_eq!(us_key('你'), None);
    }
}
