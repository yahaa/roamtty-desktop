//! Linux X11 backend: finds the user's display, grabs it over MIT-SHM, reads the cursor through
//! XFIXES, injects input with XTEST and hears of screen changes through DAMAGE.

use crate::backend::{fnv, Backend, Cursor, Frame};
use crate::keymap;
use proto::Display;
use std::collections::HashMap;
use std::path::PathBuf;
use x11rb::connection::Connection;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{self, ConnectionExt as _, ImageFormat, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

pub use crate::backend::Result;

fn err(e: impl std::fmt::Display) -> String { e.to_string() }

/// Where the user's X server is. `DISPLAY` and `XAUTHORITY` from the user's own desktop processes
/// win; a socket in /tmp/.X11-unix the user owns (Xvfb, startx) comes next.
pub fn find_display() -> Option<(String, Option<PathBuf>)> {
    if let Ok(d) = std::env::var("DISPLAY") {
        if !d.is_empty() {
            return Some((d, std::env::var_os("XAUTHORITY").map(PathBuf::from)));
        }
    }
    let uid = unsafe { libc::getuid() };
    let mut found: Option<(String, Option<PathBuf>)> = None;
    if let Ok(dir) = std::fs::read_dir("/proc") {
        for e in dir.flatten() {
            let path = e.path();
            if !e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()) { continue; }
            if owner(&path) != Some(uid) { continue; }
            let Ok(env) = std::fs::read(path.join("environ")) else { continue };
            let mut display = None;
            let mut xauth = None;
            for kv in env.split(|b| *b == 0) {
                if let Some(v) = kv.strip_prefix(b"DISPLAY=") { display = Some(String::from_utf8_lossy(v).into_owned()); }
                if let Some(v) = kv.strip_prefix(b"XAUTHORITY=") { xauth = Some(PathBuf::from(String::from_utf8_lossy(v).into_owned())); }
            }
            if let Some(d) = display.filter(|d| d.starts_with(':')) {
                if found.is_none() || xauth.is_some() { found = Some((d, xauth)); }
                if found.as_ref().is_some_and(|f| f.1.is_some()) { break; }
            }
        }
    }
    if found.is_some() { return found; }
    let dir = std::fs::read_dir("/tmp/.X11-unix").ok()?;
    let mut sockets: Vec<String> = dir.flatten()
        .filter(|e| owner(&e.path()) == Some(uid))
        .filter_map(|e| e.file_name().to_string_lossy().strip_prefix('X').map(|n| format!(":{n}")))
        .collect();
    sockets.sort();
    let home = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".Xauthority")).filter(|p| p.exists());
    sockets.into_iter().next().map(|d| (d, home))
}

fn owner(p: &std::path::Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).ok().map(|m| m.uid())
}

pub struct Shm { seg: u32, ptr: *mut u8, size: usize }

impl Drop for Shm {
    fn drop(&mut self) {
        unsafe { libc::shmdt(self.ptr as *const libc::c_void); }
    }
}

pub struct X11 {
    pub conn: RustConnection,
    root: Window,
    pub width: u16,
    pub height: u16,
    shm: Option<Shm>,
    keymap: Vec<u32>,
    min_keycode: u8,
    per_keycode: u8,
    spare: Vec<u8>,
    wheel: (i32, i32),
    cursor_serial: u32,
    clip: Option<Clipboard>,
    /// X keycode per HID usage on this server.
    keycodes: Vec<u8>,
    damage: Option<Damage>,
}

type Wake = Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>;

/// What has been drawn on the screen since the last capture, from the DAMAGE extension: a thread
/// on a second connection hears that something was (once per capture: the level is NonEmpty and
/// each capture empties it) and wakes `serve`, which used to see a change only at its next tick,
/// up to a frame interval later. The capture takes the region itself, so a still screen is neither
/// grabbed nor compared (8 MB a tick at 1080p), and a change on another monitor is not this one's.
struct Damage {
    id: damage::Damage,
    parts: u32,
    dirty: Arc<AtomicBool>,
    wake: Wake,
    /// Set by the thread when its connection broke: from then on every tick captures and compares.
    lost: Arc<AtomicBool>,
    /// The first capture has nothing to be unchanged from.
    fresh: bool,
    /// Every 8th row of the last capture, and when a still screen was last captured anyway to check
    /// it against them: an X server that draws without reporting it (a driver bug) gets caught.
    rows: Vec<u8>,
    checked: std::time::Instant,
    /// Checks in a row that found pixels changed where DAMAGE said nothing was.
    misses: u8,
}

impl Damage {
    fn start(display: &str, root: Window) -> Result<Damage> {
        let (conn, _) = x11rb::connect(Some(display)).map_err(err)?;
        conn.damage_query_version(1, 1).map_err(err)?.reply().map_err(err)?;
        conn.xfixes_query_version(5, 0).map_err(err)?.reply().map_err(err)?;
        let id = conn.generate_id().map_err(err)?;
        conn.damage_create(id, root, damage::ReportLevel::NON_EMPTY).map_err(err)?.check().map_err(err)?;
        let parts = conn.generate_id().map_err(err)?;
        conn.xfixes_create_region(parts, &[]).map_err(err)?.check().map_err(err)?;
        let dirty = Arc::new(AtomicBool::new(true));
        let lost = Arc::new(AtomicBool::new(false));
        let wake: Wake = Arc::default();
        let (d, l, w) = (dirty.clone(), lost.clone(), wake.clone());
        std::thread::spawn(move || {
            // The damage and the region belong to this connection; it lives as long as the process.
            loop {
                match conn.wait_for_event() {
                    Ok(x11rb::protocol::Event::DamageNotify(_)) => {
                        if !d.swap(true, Ordering::SeqCst) {
                            if let Some(f) = w.lock().unwrap().as_ref() { f(); }
                        }
                    }
                    Ok(_) => {}
                    Err(e) => { crate::log!("damage watch ended: {e}"); l.store(true, Ordering::SeqCst); return; }
                }
            }
        });
        Ok(Damage { id, parts, dirty, wake, lost, fresh: true, rows: Vec::new(), checked: std::time::Instant::now(), misses: 0 })
    }

    fn check_due(&self) -> bool { self.checked.elapsed() >= std::time::Duration::from_secs(2) }

    /// Every 8th row of a frame, for [Damage::rows].
    fn sample(bgra: &[u8], stride: usize, h: usize, out: &mut Vec<u8>) {
        out.clear();
        for r in (0..h).step_by(8) { out.extend_from_slice(&bgra[r * stride..(r + 1) * stride]); }
    }
}

impl X11 {
    pub fn open() -> Result<X11> {
        let (display, xauth) = find_display().ok_or("no X display for this user")?;
        std::env::set_var("DISPLAY", &display);
        if let Some(x) = xauth { std::env::set_var("XAUTHORITY", x); }
        let (conn, screen) = x11rb::connect(Some(&display)).map_err(err)?;
        let s = &conn.setup().roots[screen];
        let (root, width, height) = (s.root, s.width_in_pixels, s.height_in_pixels);
        conn.xfixes_query_version(5, 0).map_err(err)?.reply().map_err(err)?;
        conn.xtest_get_version(2, 2).map_err(err)?.reply().map_err(err)?;
        let min_keycode = conn.setup().min_keycode;
        let max_keycode = conn.setup().max_keycode;
        let map = conn.get_keyboard_mapping(min_keycode, max_keycode - min_keycode + 1).map_err(err)?.reply().map_err(err)?;
        let per = map.keysyms_per_keycode;
        // Keycodes with no symbols at all are borrowed for characters the layout lacks. Several, used in
        // turn, so a client still reading one remap never sees the next character's.
        let spare: Vec<u8> = (min_keycode..=max_keycode).rev().filter(|k| {
            let i = (*k - min_keycode) as usize * per as usize;
            map.keysyms[i..i + per as usize].iter().all(|s| *s == 0)
        }).take(8).collect();
        let mut x = X11 { conn, root, width, height, shm: None, keymap: map.keysyms, min_keycode, per_keycode: per, spare, wheel: (0, 0), cursor_serial: 0, clip: None, keycodes: Vec::new(), damage: None };
        x.keycodes = (0..=0xFFu16).map(|hid| x.resolve(hid)).collect();
        x.shm = x.make_shm().ok();
        x.clip = Clipboard::start().ok();
        // Subtracting from the watch's damage on this connection needs the version said here too.
        let damage = x.conn.damage_query_version(1, 1).map_err(err).and_then(|c| c.reply().map_err(err)).and_then(|_| Damage::start(&display, root));
        match damage {
            Ok(d) => x.damage = Some(d),
            Err(e) => crate::log!("no DAMAGE, comparing pixels: {e}"),
        }
        Ok(x)
    }

    fn make_shm(&mut self) -> Result<Shm> {
        self.conn.shm_query_version().map_err(err)?.reply().map_err(err)?;
        let size = self.width as usize * self.height as usize * 4;
        let id = unsafe { libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o600) };
        if id < 0 { return Err("shmget failed".into()); }
        let ptr = unsafe { libc::shmat(id, std::ptr::null(), 0) };
        // Marked for removal at once: it lives until both sides detach, and never leaks.
        unsafe { libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut()); }
        if ptr as isize == -1 { return Err("shmat failed".into()); }
        let seg = self.conn.generate_id().map_err(err)?;
        // read_only = false: the server writes the image into it.
        self.conn.shm_attach(seg, id as u32, false).map_err(err)?.check().map_err(err)?;
        Ok(Shm { seg, ptr: ptr as *mut u8, size })
    }

    pub fn displays(&self) -> Vec<Display> {
        let monitors = self.conn.randr_get_monitors(self.root, true).ok().and_then(|c| c.reply().ok());
        let mut out: Vec<Display> = monitors.map(|m| m.monitors).unwrap_or_default().into_iter().enumerate().map(|(i, m)| {
            let name = self.conn.get_atom_name(m.name).ok().and_then(|c| c.reply().ok())
                .map(|r| String::from_utf8_lossy(&r.name).into_owned()).unwrap_or_else(|| format!("screen {i}"));
            Display { id: i as u8, w: m.width as u32, h: m.height as u32, scale: 1.0, x: m.x as i32, y: m.y as i32, primary: m.primary, name }
        }).collect();
        if out.is_empty() {
            out.push(Display { id: 0, w: self.width as u32, h: self.height as u32, scale: 1.0, x: 0, y: 0, primary: true, name: "screen".into() });
        }
        if !out.iter().any(|d| d.primary) { out[0].primary = true; }
        out
    }

    /// True when nothing was drawn since the last capture, so `serve` need not grab the screen.
    pub fn idle(&self) -> bool {
        self.damage.as_ref().is_some_and(|d| !d.fresh && !d.check_due() && !d.lost.load(Ordering::SeqCst) && !d.dirty.load(Ordering::SeqCst))
    }

    /// Whether DAMAGE saw anything drawn on `d` since the last call, emptying what it holds. Sent
    /// before the image is taken: drawing after it starts the next round (and wakes `serve` again).
    fn damaged(&mut self, d: &Display) -> Option<bool> {
        let dm = self.damage.as_mut().filter(|dm| !dm.lost.load(Ordering::SeqCst))?;
        dm.dirty.store(false, Ordering::SeqCst);
        let r = self.conn.damage_subtract(dm.id, x11rb::NONE, dm.parts).map_err(err)
            .and_then(|_| self.conn.xfixes_fetch_region(dm.parts).map_err(err)).and_then(|c| c.reply().map_err(err));
        let region = match r {
            Ok(r) => r,
            Err(e) => { crate::log!("damage failed, comparing pixels: {e}"); self.damage = None; return None; }
        };
        let fresh = std::mem::take(&mut dm.fresh);
        let (x0, y0, x1, y1) = (d.x, d.y, d.x + d.w as i32, d.y + d.h as i32);
        let hit = region.rectangles.iter().any(|r| {
            let (rx, ry) = (r.x as i32, r.y as i32);
            rx < x1 && ry < y1 && rx + r.width as i32 > x0 && ry + r.height as i32 > y0
        });
        Some(fresh || hit)
    }

    /// Grabs one display region; `buf` keeps the copy when MIT-SHM is missing or refused.
    pub fn capture<'a>(&'a mut self, d: &Display, buf: &'a mut Vec<u8>) -> Result<Frame<'a>> {
        let changed = self.damaged(d);
        let (w, h) = (d.w as u16, d.h as u16);
        let n = w as usize * h as usize * 4;
        let mut shm_ok = false;
        if self.shm.as_ref().is_some_and(|s| n <= s.size) {
            let shm = self.shm.as_ref().unwrap();
            let r = self.conn.shm_get_image(self.root, d.x as i16, d.y as i16, w, h, !0, ImageFormat::Z_PIXMAP.into(), shm.seg, 0)
                .map_err(err).and_then(|c| c.reply().map_err(err));
            match r {
                Ok(_) => shm_ok = true,
                Err(e) => {
                    // A remote or sandboxed server can refuse shared memory: plain GetImage from now on.
                    crate::log!("MIT-SHM refused, falling back: {e}");
                    self.shm = None;
                }
            }
        }
        if !shm_ok {
            let r = self.conn.get_image(ImageFormat::Z_PIXMAP, self.root, d.x as i16, d.y as i16, w, h, !0).map_err(err)?.reply().map_err(err)?;
            *buf = r.data;
        }
        let bgra: &[u8] = if shm_ok { unsafe { std::slice::from_raw_parts(self.shm.as_ref().unwrap().ptr, n) } } else { buf };
        let stride = w as usize * 4;
        let changed = match (changed, self.damage.as_mut()) {
            (Some(c), Some(dm)) => {
                if c {
                    Damage::sample(bgra, stride, h as usize, &mut dm.rows);
                    dm.misses = 0;
                } else if dm.check_due() {
                    let mut now = Vec::new();
                    Damage::sample(bgra, stride, h as usize, &mut now);
                    dm.misses = if now != dm.rows { dm.misses + 1 } else { 0 };
                    dm.rows = now;
                }
                dm.checked = std::time::Instant::now();
                if dm.misses >= 2 {
                    crate::log!("DAMAGE missed changes twice, comparing pixels from now on");
                    self.damage = None;
                    None
                } else {
                    Some(c || dm.misses > 0)
                }
            }
            _ => None,
        };
        Ok(Frame { bgra, w: w as usize, h: h as usize, stride, changed, shown_us: None })
    }

    pub fn cursor(&mut self) -> Result<Cursor> {
        let c = self.conn.xfixes_get_cursor_image().map_err(err)?.reply().map_err(err)?;
        let shape = (c.cursor_serial != self.cursor_serial).then(|| {
            let mut rgba = Vec::with_capacity(c.cursor_image.len() * 4);
            for p in &c.cursor_image {
                // XFIXES gives premultiplied ARGB; the app wants straight RGBA.
                let a = (p >> 24) & 0xFF;
                let un = |v: u32| if a == 0 { 0 } else { ((v * 255 + a / 2) / a).min(255) as u8 };
                rgba.extend([un((p >> 16) & 0xFF), un((p >> 8) & 0xFF), un(p & 0xFF), a as u8]);
            }
            let hash = fnv(&rgba) ^ ((c.width as u64) << 48 | (c.height as u64) << 32 | (c.xhot as u64) << 16 | c.yhot as u64);
            (hash, c.width, c.height, c.xhot, c.yhot, rgba)
        });
        self.cursor_serial = c.cursor_serial;
        Ok(Cursor { x: c.x as i32, y: c.y as i32, shape, serial: c.cursor_serial })
    }

    fn fake(&self, ty: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.conn.xtest_fake_input(ty, detail, 0, self.root, x, y, 0).map_err(err)?;
        Ok(())
    }

    pub fn flush(&self) { let _ = self.conn.flush(); }

    pub fn move_abs(&self, x: i32, y: i32) -> Result<()> { self.fake(xproto::MOTION_NOTIFY_EVENT, 0, x as i16, y as i16) }

    pub fn move_rel(&self, dx: i16, dy: i16) -> Result<()> { self.fake(xproto::MOTION_NOTIFY_EVENT, 1, dx, dy) }

    pub fn button(&self, button: u8, down: bool) -> Result<()> {
        let b = match button { 1 => 1, 2 => 3, 3 => 2, _ => return Ok(()) };
        self.fake(if down { xproto::BUTTON_PRESS_EVENT } else { xproto::BUTTON_RELEASE_EVENT }, b, 0, 0)
    }

    /// Wheel units are 1/120 of a notch; X only has whole notches, so the rest carries over.
    pub fn wheel(&mut self, dx: i16, dy: i16) -> Result<()> {
        self.wheel.0 += dx as i32;
        self.wheel.1 += dy as i32;
        while self.wheel.1.abs() >= 120 {
            let up = self.wheel.1 > 0;
            self.click(if up { 4 } else { 5 })?;
            self.wheel.1 -= if up { 120 } else { -120 };
        }
        while self.wheel.0.abs() >= 120 {
            let left = self.wheel.0 < 0;
            self.click(if left { 6 } else { 7 })?;
            self.wheel.0 += if left { 120 } else { -120 };
        }
        Ok(())
    }

    fn click(&self, b: u8) -> Result<()> {
        self.fake(xproto::BUTTON_PRESS_EVENT, b, 0, 0)?;
        self.fake(xproto::BUTTON_RELEASE_EVENT, b, 0, 0)
    }

    pub fn key(&self, hid: u16, down: bool) -> Result<()> {
        match self.keycodes.get(hid as usize) {
            Some(&code) if code != 0 => self.keycode(code, down),
            _ => Ok(()),
        }
    }

    /// A key's keycode: evdev + 8 by position, unless the server numbers its keys otherwise
    /// (xfree86 codes under xrdp) and the key is a named one found by its keysym instead.
    fn resolve(&self, hid: u16) -> u8 {
        let Some(evdev) = keymap::hid_to_evdev(hid) else { return 0 };
        let code = (evdev + 8) as u8;
        let Some(sym) = keymap::hid_keysym(hid) else { return code };
        let i = code.wrapping_sub(self.min_keycode) as usize * self.per_keycode as usize;
        if code >= self.min_keycode && self.keymap.get(i) == Some(&sym) { return code; }
        self.lookup(sym).map(|(c, _)| c).unwrap_or(code)
    }

    fn keycode(&self, code: u8, down: bool) -> Result<()> {
        self.fake(if down { xproto::KEY_PRESS_EVENT } else { xproto::KEY_RELEASE_EVENT }, code, 0, 0)
    }

    fn lookup(&self, keysym: u32) -> Option<(u8, bool)> {
        let per = self.per_keycode as usize;
        self.keymap.chunks(per).enumerate().find_map(|(i, syms)| {
            let code = self.min_keycode + i as u8;
            if syms.first() == Some(&keysym) { Some((code, false)) } else if syms.get(1) == Some(&keysym) { Some((code, true)) } else { None }
        })
    }

    /// Types text: characters on the current layout go through their own keys, the rest through a
    /// spare keycode remapped to the character for one press (what xdotool does).
    pub fn text(&mut self, s: &str) -> Result<()> {
        let shift = keymap::hid_to_evdev(keymap::HID_LEFT_SHIFT).unwrap() as u8 + 8;
        let mut used = 0usize;
        for ch in s.chars() {
            let keysym = match ch {
                '\n' | '\r' => 0xFF0D,
                '\t' => 0xFF09,
                '\u{8}' => 0xFF08,
                c if (' '..='~').contains(&c) || ('\u{A0}'..='\u{FF}').contains(&c) => c as u32,
                c => 0x0100_0000 | c as u32,
            };
            if let Some((code, shifted)) = self.lookup(keysym) {
                if shifted { self.keycode(shift, true)?; }
                self.keycode(code, true)?;
                self.keycode(code, false)?;
                if shifted { self.keycode(shift, false)?; }
            } else if !self.spare.is_empty() {
                let spare = self.spare[used % self.spare.len()];
                if used >= self.spare.len() {
                    // Every spare is taken: let clients catch up before one is reused.
                    std::thread::sleep(std::time::Duration::from_millis(30));
                }
                used += 1;
                let syms = vec![keysym; self.per_keycode as usize];
                self.conn.change_keyboard_mapping(1, spare, self.per_keycode, &syms).map_err(err)?;
                self.conn.get_input_focus().map_err(err)?.reply().map_err(err)?;
                // Clients apply the new mapping when MappingNotify reaches them; give that a moment.
                std::thread::sleep(std::time::Duration::from_millis(12));
                self.keycode(spare, true)?;
                self.keycode(spare, false)?;
                self.conn.get_input_focus().map_err(err)?.reply().map_err(err)?;
            }
        }
        if used > 0 {
            // The mappings go back to empty only after the clients read the last key.
            std::thread::sleep(std::time::Duration::from_millis(40));
            let empty = vec![0u32; self.per_keycode as usize];
            for k in self.spare.iter().take(used) {
                self.conn.change_keyboard_mapping(1, *k, self.per_keycode, &empty).map_err(err)?;
            }
        }
        self.flush();
        Ok(())
    }

    pub fn tap(&self, hids: &[u16]) -> Result<()> {
        for h in hids { self.key(*h, true)?; }
        for h in hids.iter().rev() { self.key(*h, false)?; }
        self.flush();
        Ok(())
    }
}

/// CLIPBOARD access on its own connection and thread, so serving selection requests never
/// waits on the capture loop.
pub struct Clipboard {
    tx: std::sync::mpsc::Sender<ClipCmd>,
}

enum ClipCmd {
    Set(String),
    Get(std::sync::mpsc::Sender<Option<String>>),
}

impl Clipboard {
    pub fn start() -> Result<Clipboard> {
        let display = std::env::var("DISPLAY").map_err(err)?;
        let (conn, screen) = x11rb::connect(Some(&display)).map_err(err)?;
        let root = conn.setup().roots[screen].root;
        let win = conn.generate_id().map_err(err)?;
        conn.create_window(0, win, root, 0, 0, 1, 1, 0, xproto::WindowClass::INPUT_ONLY, 0,
            &xproto::CreateWindowAux::new().event_mask(xproto::EventMask::PROPERTY_CHANGE)).map_err(err)?;
        let atom = |n: &str| conn.intern_atom(false, n.as_bytes()).ok().and_then(|c| c.reply().ok()).map(|r| r.atom).unwrap_or(0);
        let atoms: HashMap<&str, u32> = ["CLIPBOARD", "UTF8_STRING", "TARGETS", "ROAMTTY_CLIP", "TEXT", "STRING"].iter().map(|n| (*n, atom(n))).collect();
        conn.flush().map_err(err)?;
        let (tx, rx) = std::sync::mpsc::channel::<ClipCmd>();
        std::thread::spawn(move || {
            let a = |n: &str| atoms[n];
            let mut owned: Option<String> = None;
            let mut pending: Option<std::sync::mpsc::Sender<Option<String>>> = None;
            let mut asked = std::time::Instant::now();
            loop {
                match rx.recv_timeout(std::time::Duration::from_millis(20)) {
                    Ok(ClipCmd::Set(s)) => {
                        owned = Some(s);
                        let _ = conn.set_selection_owner(win, a("CLIPBOARD"), x11rb::CURRENT_TIME);
                        let _ = conn.flush();
                    }
                    Ok(ClipCmd::Get(reply)) => {
                        if let Some(s) = &owned {
                            let _ = reply.send(Some(s.clone()));
                        } else {
                            let _ = conn.convert_selection(win, a("CLIPBOARD"), a("UTF8_STRING"), a("ROAMTTY_CLIP"), x11rb::CURRENT_TIME);
                            let _ = conn.flush();
                            pending = Some(reply);
                            asked = std::time::Instant::now();
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                    Err(_) => {}
                }
                if pending.is_some() && asked.elapsed().as_secs() >= 2 {
                    let _ = pending.take().unwrap().send(None);
                }
                while let Ok(Some(ev)) = conn.poll_for_event() {
                    use x11rb::protocol::Event;
                    match ev {
                        Event::SelectionNotify(e) => {
                            let text = (e.property != 0).then(|| {
                                conn.get_property(true, win, a("ROAMTTY_CLIP"), 0u32, 0, 1 << 22).ok()
                                    .and_then(|c| c.reply().ok()).map(|r| String::from_utf8_lossy(&r.value).into_owned())
                            }).flatten();
                            if let Some(p) = pending.take() { let _ = p.send(text); }
                        }
                        Event::SelectionClear(_) => owned = None,
                        Event::SelectionRequest(e) => {
                            let mut property = e.property;
                            match (&owned, e.target) {
                                (Some(_), t) if t == a("TARGETS") => {
                                    let targets = [a("TARGETS"), a("UTF8_STRING"), a("TEXT"), a("STRING")];
                                    let _ = conn.change_property32(xproto::PropMode::REPLACE, e.requestor, e.property, xproto::AtomEnum::ATOM, &targets);
                                }
                                (Some(s), t) if t == a("UTF8_STRING") || t == a("TEXT") || t == a("STRING") => {
                                    let _ = conn.change_property8(xproto::PropMode::REPLACE, e.requestor, e.property, t, s.as_bytes());
                                }
                                _ => property = 0,
                            }
                            let notify = xproto::SelectionNotifyEvent {
                                response_type: xproto::SELECTION_NOTIFY_EVENT, sequence: 0, time: e.time,
                                requestor: e.requestor, selection: e.selection, target: e.target, property,
                            };
                            let _ = conn.send_event(false, e.requestor, xproto::EventMask::NO_EVENT, notify);
                            let _ = conn.flush();
                        }
                        _ => {}
                    }
                }
            }
        });
        Ok(Clipboard { tx })
    }

    pub fn set(&self, s: String) { let _ = self.tx.send(ClipCmd::Set(s)); }

    pub fn get(&self) -> Option<String> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.tx.send(ClipCmd::Get(tx)).ok()?;
        rx.recv_timeout(std::time::Duration::from_secs(3)).ok().flatten()
    }
}

impl Backend for X11 {
    fn kind(&self) -> &'static str { "x11" }
    fn displays(&self) -> Vec<Display> { X11::displays(self) }
    fn capture<'a>(&'a mut self, d: &Display, buf: &'a mut Vec<u8>) -> Result<Frame<'a>> { X11::capture(self, d, buf) }
    fn idle(&mut self) -> bool { X11::idle(self) }
    fn on_frame(&mut self, wake: Arc<dyn Fn() + Send + Sync>) {
        if let Some(d) = &self.damage { *d.wake.lock().unwrap() = Some(wake); }
    }
    fn cursor(&mut self) -> Option<Cursor> { X11::cursor(self).ok() }
    fn move_abs(&mut self, x: i32, y: i32) -> Result<()> { X11::move_abs(self, x, y) }
    fn move_rel(&mut self, dx: i16, dy: i16) -> Result<()> { X11::move_rel(self, dx, dy) }
    fn button(&mut self, button: u8, down: bool) -> Result<()> { X11::button(self, button, down) }
    fn wheel(&mut self, dx: i16, dy: i16) -> Result<()> { X11::wheel(self, dx, dy) }
    fn key(&mut self, hid: u16, down: bool) -> Result<()> { X11::key(self, hid, down) }
    fn text(&mut self, s: &str) -> Result<()> { X11::text(self, s) }
    fn tap(&mut self, hids: &[u16]) -> Result<()> { X11::tap(self, hids) }
    fn flush(&mut self) { X11::flush(self) }
    fn clip_set(&mut self, text: String) { if let Some(c) = &self.clip { c.set(text); } }
    fn clip_get(&mut self) -> Option<String> { self.clip.as_ref().and_then(|c| c.get()) }
}
