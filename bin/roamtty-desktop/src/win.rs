//! Windows backend: DXGI Desktop Duplication for frames, SendInput for input (scan codes for keys,
//! KEYEVENTF_UNICODE for text), the Win32 clipboard. It runs in the agent, which a scheduled task
//! starts in the user's interactive session: OpenSSH's own processes live in Session 0, which has no
//! desktop. The process is Per-Monitor DPI aware v2, so every coordinate is a physical pixel.

use crate::backend::{Backend, Cursor, Frame, Result};
use proto::Display;
use windows::core::Interface;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, HGLOBAL, HANDLE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_OUTDUPL_FRAME_INFO, DXGI_ERROR_WAIT_TIMEOUT};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW, MONITORINFO};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Shutdown::LockWorkStation;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN};
use windows::core::BOOL;

fn err(e: impl std::fmt::Display) -> String { e.to_string() }

const CF_UNICODETEXT: u32 = 13;

struct Dup { name: String, dup: IDXGIOutputDuplication, staging: Option<ID3D11Texture2D>, w: u32, h: u32 }

pub struct Win {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    displays: Vec<(Display, HMONITOR)>,
    dups: Vec<Dup>,
    last_cursor: Option<(i32, i32)>,
}

/// USB HID usage to a set-1 scan code, with the E0 (extended) flag where the key has one.
fn hid_to_scan(hid: u16) -> Option<(u16, bool)> {
    // Same physical keys as Linux evdev codes, which are set-1 scan codes for the main block.
    let code = crate::keymap::hid_to_evdev(hid)?;
    let extended = matches!(hid, 0x46..=0x52 | 0x54 | 0x58 | 0x65 | 0xE3..=0xE7);
    let scan = match code {
        102 => 0x47, 104 => 0x49, 107 => 0x4F, 109 => 0x51, 110 => 0x52, 111 => 0x53, // Home PgUp End PgDn Ins Del
        103 => 0x48, 105 => 0x4B, 106 => 0x4D, 108 => 0x50, // arrows
        96 => 0x1C, 97 => 0x1D, 98 => 0x35, 100 => 0x38, 125 => 0x5B, 126 => 0x5C, 127 => 0x5D, 99 => 0x37, 119 => 0x45,
        c if c < 0x59 => c,
        _ => return None,
    };
    Some((scan, extended))
}

fn send(inputs: &[INPUT]) {
    unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
}

fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 } } }
}

fn keyboard(scan: u16, vk: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } } }
}

unsafe extern "system" fn monitor(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
    let list = unsafe { &mut *(data.0 as *mut Vec<(HMONITOR, RECT, String, bool)>) };
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if unsafe { GetMonitorInfoW(h, &mut info as *mut _ as *mut MONITORINFO) }.as_bool() {
        let name = String::from_utf16_lossy(&info.szDevice).trim_end_matches('\0').to_string();
        list.push((h, info.monitorInfo.rcMonitor, name, info.monitorInfo.dwFlags & 1 != 0));
    }
    BOOL(1)
}

impl Win {
    pub fn open() -> Result<Win> {
        unsafe { let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2); }
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(None, D3D_DRIVER_TYPE_HARDWARE, Default::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT, None,
                D3D11_SDK_VERSION, Some(&mut device), None, Some(&mut context)).map_err(err)?;
        }
        let (device, context) = (device.ok_or("no D3D11 device")?, context.ok_or("no D3D11 context")?);
        let mut list: Vec<(HMONITOR, RECT, String, bool)> = Vec::new();
        unsafe { let _ = EnumDisplayMonitors(None, None, Some(monitor), LPARAM(&mut list as *mut _ as isize)); }
        if list.is_empty() { return Err("no display (is anyone logged in?)".into()); }
        let displays = list.iter().enumerate().map(|(i, (h, r, name, primary))| (Display {
            id: i as u8, w: (r.right - r.left) as u32, h: (r.bottom - r.top) as u32, scale: 1.0, x: r.left, y: r.top,
            primary: *primary, name: name.trim_start_matches("\\\\.\\").to_string(),
        }, *h)).collect();
        Ok(Win { device, context, displays, dups: Vec::new(), last_cursor: None })
    }

    /// The duplication of the output whose device name matches `name`.
    fn duplication(&mut self, name: &str) -> Result<usize> {
        if let Some(i) = self.dups.iter().position(|d| d.name == name) { return Ok(i); }
        let dxgi: IDXGIDevice = self.device.cast().map_err(err)?;
        let adapter = unsafe { dxgi.GetAdapter() }.map_err(err)?;
        for i in 0.. {
            let Ok(output) = (unsafe { adapter.EnumOutputs(i) }) else { break };
            let desc = unsafe { output.GetDesc() }.map_err(err)?;
            let dev = String::from_utf16_lossy(&desc.DeviceName).trim_end_matches('\0').to_string();
            if !dev.ends_with(name) { continue; }
            let o1: IDXGIOutput1 = output.cast().map_err(err)?;
            let dup = unsafe { o1.DuplicateOutput(&self.device) }.map_err(err)?;
            let r = desc.DesktopCoordinates;
            self.dups.push(Dup { name: name.to_string(), dup, staging: None, w: (r.right - r.left) as u32, h: (r.bottom - r.top) as u32 });
            return Ok(self.dups.len() - 1);
        }
        Err(format!("no DXGI output for {name}"))
    }
}

impl Backend for Win {
    fn kind(&self) -> &'static str { "windows" }

    fn displays(&self) -> Vec<Display> { self.displays.iter().map(|(d, _)| d.clone()).collect() }

    fn capture<'a>(&'a mut self, d: &Display, buf: &'a mut Vec<u8>) -> Result<Frame<'a>> {
        let i = self.duplication(&d.name.clone())?;
        let (w, h) = (self.dups[i].w, self.dups[i].h);
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe { self.dups[i].dup.AcquireNextFrame(50, &mut info, &mut resource) } {
            Ok(()) => {}
            // Nothing changed: the last frame stands.
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                if buf.is_empty() { return Err("no frame yet".into()); }
                return Ok(Frame { bgra: buf, w: w as usize, h: h as usize, stride: w as usize * 4 });
            }
            // Access lost (secure desktop, mode change): start over next time.
            Err(e) => { self.dups.remove(i); return Err(err(e)); }
        }
        let tex: ID3D11Texture2D = resource.ok_or("no frame resource")?.cast().map_err(err)?;
        if self.dups[i].staging.is_none() {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { tex.GetDesc(&mut desc) };
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            desc.MiscFlags = 0;
            let mut staging = None;
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut staging)) }.map_err(err)?;
            self.dups[i].staging = staging;
        }
        let staging = self.dups[i].staging.clone().unwrap();
        unsafe {
            self.context.CopyResource(&staging, &tex);
            let _ = self.dups[i].dup.ReleaseFrame();
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).map_err(err)?;
            buf.resize((w * h * 4) as usize, 0);
            for row in 0..h as usize {
                let src = (mapped.pData as *const u8).add(row * mapped.RowPitch as usize);
                std::ptr::copy_nonoverlapping(src, buf.as_mut_ptr().add(row * w as usize * 4), w as usize * 4);
            }
            self.context.Unmap(&staging, 0);
        }
        Ok(Frame { bgra: buf, w: w as usize, h: h as usize, stride: w as usize * 4 })
    }

    fn cursor(&mut self) -> Option<Cursor> {
        let mut p = POINT::default();
        unsafe { GetCursorPos(&mut p) }.ok()?;
        let first = self.last_cursor.is_none();
        self.last_cursor = Some((p.x, p.y));
        Some(Cursor { x: p.x, y: p.y, shape: first.then(|| (0xA770_u64, 0, 0, 0, 0, Vec::new())), serial: 0 })
    }

    fn move_abs(&mut self, x: i32, y: i32) -> Result<()> {
        // Absolute moves are 0..65535 across the whole virtual desktop.
        let (vx, vy, vw, vh) = unsafe { (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1), GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1)) };
        let nx = ((x - vx) as i64 * 65535 / (vw - 1).max(1) as i64) as i32;
        let ny = ((y - vy) as i64 * 65535 / (vh - 1).max(1) as i64) as i32;
        send(&[mouse(nx, ny, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)]);
        Ok(())
    }

    fn move_rel(&mut self, dx: i16, dy: i16) -> Result<()> { send(&[mouse(dx as i32, dy as i32, 0, MOUSEEVENTF_MOVE)]); Ok(()) }

    fn button(&mut self, button: u8, down: bool) -> Result<()> {
        let f = match (button, down) {
            (1, true) => MOUSEEVENTF_LEFTDOWN, (1, false) => MOUSEEVENTF_LEFTUP,
            (2, true) => MOUSEEVENTF_RIGHTDOWN, (2, false) => MOUSEEVENTF_RIGHTUP,
            (3, true) => MOUSEEVENTF_MIDDLEDOWN, (3, false) => MOUSEEVENTF_MIDDLEUP,
            _ => return Ok(()),
        };
        send(&[mouse(0, 0, 0, f)]);
        Ok(())
    }

    /// Windows counts wheels in 1/120 of a notch too (WHEEL_DELTA), positive away from the user.
    fn wheel(&mut self, dx: i16, dy: i16) -> Result<()> {
        if dy != 0 { send(&[mouse(0, 0, dy as i32, MOUSEEVENTF_WHEEL)]); }
        if dx != 0 { send(&[mouse(0, 0, dx as i32, MOUSEEVENTF_HWHEEL)]); }
        Ok(())
    }

    fn key(&mut self, hid: u16, down: bool) -> Result<()> {
        let Some((scan, ext)) = hid_to_scan(hid) else { return Ok(()) };
        let mut f = KEYEVENTF_SCANCODE;
        if ext { f |= KEYEVENTF_EXTENDEDKEY; }
        if !down { f |= KEYEVENTF_KEYUP; }
        send(&[keyboard(scan, 0, f)]);
        Ok(())
    }

    /// One UTF-16 unit per event pair; a surrogate pair is two units, as SendInput wants.
    fn text(&mut self, s: &str) -> Result<()> {
        let inputs: Vec<INPUT> = s.encode_utf16().flat_map(|u| [keyboard(u, 0, KEYEVENTF_UNICODE), keyboard(u, 0, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP)]).collect();
        for chunk in inputs.chunks(64) { send(chunk); }
        Ok(())
    }

    fn tap(&mut self, hids: &[u16]) -> Result<()> {
        for h in hids { self.key(*h, true)?; }
        for h in hids.iter().rev() { self.key(*h, false)?; }
        Ok(())
    }

    fn flush(&mut self) {}

    fn clip_set(&mut self, text: String) {
        let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            if OpenClipboard(Some(HWND::default())).is_err() { return; }
            let _ = EmptyClipboard();
            if let Ok(mem) = GlobalAlloc(GMEM_MOVEABLE, units.len() * 2) {
                let p = GlobalLock(mem) as *mut u16;
                if !p.is_null() {
                    std::ptr::copy_nonoverlapping(units.as_ptr(), p, units.len());
                    let _ = GlobalUnlock(mem);
                    let _ = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(mem.0)));
                }
            }
            let _ = CloseClipboard();
        }
    }

    fn clip_get(&mut self) -> Option<String> {
        unsafe {
            OpenClipboard(Some(HWND::default())).ok()?;
            let r = GetClipboardData(CF_UNICODETEXT).ok().and_then(|h| {
                let mem = HGLOBAL(h.0);
                let p = GlobalLock(mem) as *const u16;
                if p.is_null() { return None; }
                let mut n = 0;
                while *p.add(n) != 0 { n += 1; }
                let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
                let _ = GlobalUnlock(mem);
                Some(s)
            });
            let _ = CloseClipboard();
            r
        }
    }

    /// Win+D shows the desktop, Win+Tab all windows.
    fn action(&mut self, action: u8) -> Result<()> {
        match action {
            proto::action::SHOW_DESKTOP => self.tap(&[0xE3, 0x07]),
            proto::action::ALL_WINDOWS => self.tap(&[0xE3, 0x2B]),
            _ => Ok(()),
        }
    }

    fn lock_screen(&mut self) { unsafe { let _ = LockWorkStation(); } }
}
