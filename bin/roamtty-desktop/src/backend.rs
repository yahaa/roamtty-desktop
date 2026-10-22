//! What `serve` needs from a desktop: displays, frames, the cursor, input and the clipboard.
//! X11 and wlroots Wayland implement it on Linux; macOS and Windows have their own.

use proto::Display;

pub type Result<T> = std::result::Result<T, String>;

pub struct Frame<'a> {
    pub bgra: &'a [u8],
    pub w: usize,
    pub h: usize,
    pub stride: usize,
}

pub struct Cursor {
    pub x: i32,
    pub y: i32,
    /// Set when the shape changed since the last call: hash, w, h, hot_x, hot_y, RGBA.
    pub shape: Option<(u64, u16, u16, u16, u16, Vec<u8>)>,
    pub serial: u32,
}

pub trait Backend {
    fn kind(&self) -> &'static str;
    fn displays(&self) -> Vec<Display>;
    /// One display's pixels as BGRA; `buf` holds them when the backend cannot lend its own memory.
    fn capture<'a>(&'a mut self, d: &Display, buf: &'a mut Vec<u8>) -> Result<Frame<'a>>;
    /// None when the backend draws the cursor into the frames instead.
    fn cursor(&mut self) -> Option<Cursor>;
    fn move_abs(&mut self, x: i32, y: i32) -> Result<()>;
    fn move_rel(&mut self, dx: i16, dy: i16) -> Result<()>;
    fn button(&mut self, button: u8, down: bool) -> Result<()>;
    /// 1/120 of a notch; positive dy scrolls up.
    fn wheel(&mut self, dx: i16, dy: i16) -> Result<()>;
    fn key(&mut self, hid: u16, down: bool) -> Result<()>;
    fn text(&mut self, s: &str) -> Result<()>;
    fn tap(&mut self, hids: &[u16]) -> Result<()>;
    fn flush(&mut self);
    fn clip_set(&mut self, text: String);
    fn clip_get(&mut self) -> Option<String>;
    /// ACTION: show the desktop or all windows. Linux sends Super+D / Super; others override.
    fn action(&mut self, action: u8) -> Result<()> {
        let keys: &[u16] = match action {
            proto::action::SHOW_DESKTOP => &[crate::keymap::HID_LEFT_GUI, crate::keymap::HID_D],
            proto::action::ALL_WINDOWS => &[crate::keymap::HID_LEFT_GUI],
            _ => &[],
        };
        self.tap(keys)
    }
    /// Wakes a blanked screen as serving starts, so the first picture is the desktop or its lock
    /// screen rather than black until the user happens to touch it: a nudge there and back.
    fn wake(&mut self) {
        if self.move_rel(1, 0).is_ok() { let _ = self.move_rel(-1, 0); }
        self.flush();
    }
    /// "断开后锁定电脑": locks the session when the connection really ends.
    fn lock_screen(&mut self) { let _ = std::process::Command::new("loginctl").arg("lock-session").status(); }
    /// What the user must allow on the host before the desktop works (macOS TCC).
    fn needs(&self) -> Vec<String> { Vec::new() }
}

#[cfg(target_os = "linux")]
pub fn fnv(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for x in b {
        h ^= *x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}
