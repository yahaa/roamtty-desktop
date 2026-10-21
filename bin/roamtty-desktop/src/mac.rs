//! macOS backend: ScreenCaptureKit for frames, VideoToolbox for H.264, CGEvent for input,
//! NSPasteboard for the clipboard. It runs inside `Roamtty Desktop.app`, started by launchd in the
//! user's GUI session, because macOS grants Screen Recording and Accessibility (TCC) to apps, never
//! to a process an SSH login started.

use crate::backend::{Backend, Cursor, Frame, Result};
use crate::encode::{to_i420, VideoEncoder};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, CGPoint};
use objc2_core_graphics::{
    CGDisplayBounds, CGDisplayPixelsWide, CGEvent, CGEventFlags, CGEventSource, CGEventTapLocation,
    CGEventType, CGGetActiveDisplayList, CGMainDisplayID, CGMouseButton, CGPreflightScreenCaptureAccess,
    CGRequestScreenCaptureAccess, CGScrollEventUnit,
};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput, SCStreamOutputType,
};
use proto::Display;
use std::ptr::NonNull;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPMAssertionDeclareUserActivity(name: *const std::ffi::c_void, kind: u32, id: *mut u32) -> i32;
    fn IOPMAssertionCreateWithName(kind: *const std::ffi::c_void, level: u32, name: *const std::ffi::c_void, id: *mut u32) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
}

/// Wakes a sleeping display (as a key press would) and keeps it on while a phone is connected:
/// a sleeping display has no picture to send. Dropped when the connection ends.
pub struct AwakeDisplay(u32);

impl AwakeDisplay {
    pub fn hold() -> AwakeDisplay {
        let name = CFString::from_static_str("Roamtty remote desktop");
        let kind = CFString::from_static_str("PreventUserIdleDisplaySleep");
        let mut wake = 0u32;
        let mut hold = 0u32;
        unsafe {
            IOPMAssertionDeclareUserActivity(CFRetained::as_ptr(&name).as_ptr() as *const _, 0, &mut wake);
            IOPMAssertionCreateWithName(CFRetained::as_ptr(&kind).as_ptr() as *const _, 255, CFRetained::as_ptr(&name).as_ptr() as *const _, &mut hold);
        }
        AwakeDisplay(hold)
    }
}

impl Drop for AwakeDisplay {
    fn drop(&mut self) { unsafe { IOPMAssertionRelease(self.0); } }
}

/// What the user still has to allow on the Mac: `screen-recording`, `accessibility`.
pub fn needs() -> Vec<String> {
    let mut n = Vec::new();
    if !CGPreflightScreenCaptureAccess() { n.push("screen-recording".to_string()); }
    if !unsafe { AXIsProcessTrusted() } { n.push("accessibility".to_string()); }
    n
}

/// Shows the system prompts once; the user answers them on the Mac.
pub fn request_permissions() {
    if !CGPreflightScreenCaptureAccess() { let _ = CGRequestScreenCaptureAccess(); }
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" { fn AXIsProcessTrustedWithOptions(options: *const std::ffi::c_void) -> bool; }
    if !unsafe { AXIsProcessTrusted() } {
        let key = CFString::from_static_str("AXTrustedCheckOptionPrompt");
        let dict = CFDictionary::<CFString, CFBoolean>::from_slices(&[&*key], &[CFBoolean::new(true)]);
        unsafe { AXIsProcessTrustedWithOptions(CFRetained::as_ptr(&dict).as_ptr() as *const _); }
    }
}

/// The newest frame ScreenCaptureKit delivered, as BGRA.
#[derive(Default)]
struct Latest { bgra: Vec<u8>, w: usize, h: usize, stride: usize, seq: u64 }

struct OutputIvars { latest: Arc<Mutex<Latest>> }

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = OutputIvars]
    struct FrameSink;

    unsafe impl NSObjectProtocol for FrameSink {}

    unsafe impl SCStreamOutput for FrameSink {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, ty: SCStreamOutputType) {
            if ty != SCStreamOutputType::Screen { return; }
            // Idle frames (nothing changed) carry no image.
            let Some(image) = (unsafe { sample.image_buffer() }) else { return };
            let pb: &CVPixelBuffer = &image;
            unsafe {
                if CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) != 0 { return; }
                let base = CVPixelBufferGetBaseAddress(pb) as *const u8;
                let stride = CVPixelBufferGetBytesPerRow(pb);
                let (w, h) = (CVPixelBufferGetWidth(pb), CVPixelBufferGetHeight(pb));
                if !base.is_null() {
                    let mut l = self.ivars().latest.lock().unwrap();
                    l.bgra.clear();
                    l.bgra.extend_from_slice(std::slice::from_raw_parts(base, stride * h));
                    l.w = w; l.h = h; l.stride = stride; l.seq += 1;
                }
                CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly);
            }
        }
    }
);

impl FrameSink {
    fn new(latest: Arc<Mutex<Latest>>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { latest });
        unsafe { msg_send![super(this), init] }
    }
}

struct Capture { stream: Retained<SCStream>, _sink: Retained<FrameSink>, display: u32, latest: Arc<Mutex<Latest>> }

pub struct Mac {
    _awake: AwakeDisplay,
    displays: Vec<(u32, Display)>,
    capture: Option<Capture>,
    source: Option<CFRetained<CGEventSource>>,
    flags: CGEventFlags,
    buttons: [bool; 3],
    wheel: (i32, i32),
    last_cursor: Option<(i32, i32)>,
    /// ScreenCaptureKit refused (TCC): its answer beats CGPreflightScreenCaptureAccess, which can disagree.
    denied: bool,
}

fn shareable() -> Result<Retained<SCShareableContent>> {
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        let r = if let Some(c) = unsafe { Retained::retain(content) } { Ok(c) }
            else { Err(unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string()).unwrap_or_else(|| "no content".into())) };
        let _ = tx.send(r);
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    rx.recv_timeout(Duration::from_secs(5)).map_err(|_| "ScreenCaptureKit did not answer".to_string())?
}

impl Mac {
    pub fn open() -> Result<Mac> {
        let awake = AwakeDisplay::hold();
        let mut ids = [0u32; 16];
        let mut n = 0u32;
        // A display that is asleep is not "active"; it wakes from the activity declared above.
        for _ in 0..20 {
            unsafe { CGGetActiveDisplayList(16, ids.as_mut_ptr(), &mut n) };
            if n > 0 { break; }
            std::thread::sleep(Duration::from_millis(100));
        }
        if n == 0 { unsafe { objc2_core_graphics::CGGetOnlineDisplayList(16, ids.as_mut_ptr(), &mut n) }; }
        let main = CGMainDisplayID();
        let displays: Vec<(u32, Display)> = ids[..n as usize].iter().enumerate().map(|(i, id)| {
            let b = CGDisplayBounds(*id);
            let pw = CGDisplayPixelsWide(*id) as u32;
            // Native pixels: the mode's backing size; points × scale on Retina.
            let scale = if b.size.width > 0.0 { native_scale(*id).unwrap_or(pw as f64 / b.size.width) } else { 1.0 };
            let (w, h) = ((b.size.width * scale).round() as u32, (b.size.height * scale).round() as u32);
            (*id, Display { id: i as u8, w, h, scale: scale as f32, x: b.origin.x as i32, y: b.origin.y as i32,
                primary: *id == main, name: if *id == main { "Built-in".into() } else { format!("Display {}", i + 1) } })
        }).collect();
        if displays.is_empty() { return Err("no display".into()); }
        let source = CGEventSource::new(objc2_core_graphics::CGEventSourceStateID::HIDSystemState);
        let mut m = Mac { _awake: awake, displays, capture: None, source, flags: CGEventFlags::empty(), buttons: [false; 3],
            wheel: (0, 0), last_cursor: None, denied: false };
        // Try the main display now, so the hello's `needs` says what ScreenCaptureKit really thinks.
        let main = m.displays.iter().map(|(_, d)| d.clone()).find(|d| d.primary).unwrap();
        if let Err(e) = m.start_capture(&main) { crate::log!("capture probe: {e}"); }
        Ok(m)
    }

    fn start_capture(&mut self, d: &Display) -> Result<()> {
        let (id, _) = self.displays.iter().find(|(_, x)| x.id == d.id).cloned().ok_or("no such display")?;
        if self.capture.as_ref().is_some_and(|c| c.display == id) { return Ok(()); }
        if let Some(c) = self.capture.take() { unsafe { c.stream.stopCaptureWithCompletionHandler(None) }; }
        let content = shareable()?;
        let displays = unsafe { content.displays() };
        let sc: Retained<SCDisplay> = displays.iter().find(|x| unsafe { x.displayID() } == id).ok_or("display not shareable")?;
        let filter = unsafe { SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &sc, &NSArray::new()) };
        let config = unsafe { SCStreamConfiguration::new() };
        unsafe {
            config.setWidth(d.w as usize);
            config.setHeight(d.h as usize);
            config.setPixelFormat(u32::from_be_bytes(*b"BGRA"));
            // The phone draws the cursor.
            config.setShowsCursor(false);
            config.setMinimumFrameInterval(CMTime { value: 1, timescale: 60, flags: CMTimeFlags::Valid, epoch: 0 });
            config.setQueueDepth(3);
        }
        let stream = unsafe { SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, None) };
        let latest = Arc::new(Mutex::new(Latest::default()));
        let sink = FrameSink::new(latest.clone());
        unsafe {
            stream.addStreamOutput_type_sampleHandlerQueue_error(ProtocolObject::from_ref(&*sink), SCStreamOutputType::Screen, None)
                .map_err(|e| e.localizedDescription().to_string())?;
        }
        let (tx, rx) = mpsc::channel();
        let done = RcBlock::new(move |e: *mut NSError| {
            let _ = tx.send(unsafe { e.as_ref() }.map(|e| e.localizedDescription().to_string()));
        });
        unsafe { stream.startCaptureWithCompletionHandler(Some(&done)) };
        if let Some(e) = rx.recv_timeout(Duration::from_secs(5)).map_err(|_| "capture did not start".to_string())? {
            // SCStreamErrorUserDeclined and friends: the user has not allowed Screen Recording.
            self.denied = true;
            return Err(e);
        }
        self.denied = false;
        self.capture = Some(Capture { stream, _sink: sink, display: id, latest });
        Ok(())
    }

    /// A point in native pixels of display `d` to global points.
    fn to_points(&self, x: i32, y: i32) -> CGPoint {
        let d = self.displays.iter().map(|(_, d)| d).find(|d| x >= d.x && y >= d.y).unwrap_or(&self.displays[0].1);
        CGPoint { x: x as f64 / d.scale as f64 + d.x as f64 - d.x as f64 / d.scale as f64 * 0.0, y: y as f64 / d.scale as f64 }
    }

    fn post(&self, e: Option<CFRetained<CGEvent>>) {
        if let Some(e) = e { CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&e)); }
    }

    fn mouse(&self, ty: CGEventType, at: CGPoint, button: CGMouseButton) {
        let e = CGEvent::new_mouse_event(self.source.as_deref(), ty, at, button);
        if let Some(e) = &e { CGEvent::set_flags(Some(e), self.flags); }
        self.post(e);
    }

    fn location(&self) -> CGPoint {
        CGEvent::new(None).map(|e| CGEvent::location(Some(&e))).unwrap_or(CGPoint { x: 0.0, y: 0.0 })
    }
}

/// Keeps a point (global points) on some display. Posted moves are not clamped by macOS: a trackpad
/// drag past the edge left the cursor off every screen, invisible here and on the phone, until dragged back.
fn clamp(displays: &[(u32, Display)], p: CGPoint) -> CGPoint {
    let bounds = |d: &Display| (d.x as f64, d.y as f64, d.x as f64 + d.w as f64 / d.scale as f64 - 1.0, d.y as f64 + d.h as f64 / d.scale as f64 - 1.0);
    if displays.iter().any(|(_, d)| { let (l, t, r, b) = bounds(d); p.x >= l && p.x <= r && p.y >= t && p.y <= b }) { return p; }
    displays.iter().map(|(_, d)| {
        let (l, t, r, b) = bounds(d);
        let q = CGPoint { x: p.x.clamp(l, r), y: p.y.clamp(t, b) };
        ((q.x - p.x).powi(2) + (q.y - p.y).powi(2), q)
    }).min_by(|a, b| a.0.total_cmp(&b.0)).map(|(_, q)| q).unwrap_or(p)
}

/// The backing scale of a display's current mode (2 on Retina).
fn native_scale(id: u32) -> Option<f64> {
    let mode = objc2_core_graphics::CGDisplayCopyDisplayMode(id)?;
    let pw = objc2_core_graphics::CGDisplayMode::pixel_width(Some(&mode)) as f64;
    let w = objc2_core_graphics::CGDisplayMode::width(Some(&mode)) as f64;
    (w > 0.0).then(|| pw / w)
}

/// USB HID usage to a macOS virtual key code (kVK_*), by physical position.
fn hid_to_vk(hid: u16) -> Option<u16> {
    const LETTERS: [u16; 26] = [0x00, 0x0B, 0x08, 0x02, 0x0E, 0x03, 0x05, 0x04, 0x22, 0x26, 0x28, 0x25, 0x2E, 0x2D, 0x1F, 0x23, 0x0C, 0x0F, 0x01, 0x11, 0x20, 0x09, 0x0D, 0x07, 0x10, 0x06];
    const DIGITS: [u16; 10] = [0x12, 0x13, 0x14, 0x15, 0x17, 0x16, 0x1A, 0x1C, 0x19, 0x1D];
    const F: [u16; 12] = [0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F];
    Some(match hid {
        0x04..=0x1D => LETTERS[(hid - 0x04) as usize],
        0x1E..=0x27 => DIGITS[(hid - 0x1E) as usize],
        0x28 => 0x24, 0x29 => 0x35, 0x2A => 0x33, 0x2B => 0x30, 0x2C => 0x31, 0x2D => 0x1B, 0x2E => 0x18, 0x2F => 0x21,
        0x30 => 0x1E, 0x31 => 0x2A, 0x33 => 0x29, 0x34 => 0x27, 0x35 => 0x32, 0x36 => 0x2B, 0x37 => 0x2F, 0x38 => 0x2C, 0x39 => 0x39,
        0x3A..=0x45 => F[(hid - 0x3A) as usize],
        0x49 => 0x72, 0x4A => 0x73, 0x4B => 0x74, 0x4C => 0x75, 0x4D => 0x77, 0x4E => 0x79,
        0x4F => 0x7C, 0x50 => 0x7B, 0x51 => 0x7D, 0x52 => 0x7E,
        0x7F => 0x4A, 0x80 => 0x48, 0x81 => 0x49,
        0xE0 => 0x3B, 0xE1 => 0x38, 0xE2 => 0x3A, 0xE3 => 0x37, 0xE4 => 0x3E, 0xE5 => 0x3C, 0xE6 => 0x3D, 0xE7 => 0x36,
        _ => return None,
    })
}

fn modifier_flag(hid: u16) -> Option<CGEventFlags> {
    Some(match hid {
        0xE0 | 0xE4 => CGEventFlags::MaskControl,
        0xE1 | 0xE5 => CGEventFlags::MaskShift,
        0xE2 | 0xE6 => CGEventFlags::MaskAlternate,
        0xE3 | 0xE7 => CGEventFlags::MaskCommand,
        _ => return None,
    })
}

impl Backend for Mac {
    fn kind(&self) -> &'static str { "macos" }

    fn displays(&self) -> Vec<Display> { self.displays.iter().map(|(_, d)| d.clone()).collect() }

    fn capture<'a>(&'a mut self, d: &Display, buf: &'a mut Vec<u8>) -> Result<Frame<'a>> {
        self.start_capture(d)?;
        let c = self.capture.as_ref().unwrap();
        // Wait briefly for the first frame; later calls take whatever is newest.
        for _ in 0..50 {
            if c.latest.lock().unwrap().seq > 0 { break; }
            std::thread::sleep(Duration::from_millis(10));
        }
        let l = c.latest.lock().unwrap();
        if l.seq == 0 { return Err("no frame yet".into()); }
        buf.clear();
        buf.extend_from_slice(&l.bgra);
        Ok(Frame { bgra: buf, w: l.w, h: l.h, stride: l.stride })
    }

    fn cursor(&mut self) -> Option<Cursor> {
        let p = self.location();
        let d = self.displays.iter().map(|(_, d)| d).find(|d| p.x >= d.x as f64 && p.y >= d.y as f64).unwrap_or(&self.displays[0].1);
        let (x, y) = (((p.x - d.x as f64) * d.scale as f64) as i32 + d.x, ((p.y - d.y as f64) * d.scale as f64) as i32 + d.y);
        let first = self.last_cursor.is_none();
        self.last_cursor = Some((x, y));
        // The arrow, once: macOS gives no public API for the current cursor image outside AppKit's own process.
        let shape = first.then(|| (0xA770_u64, 0, 0, 0, 0, Vec::new()));
        Some(Cursor { x, y, shape, serial: 0 })
    }

    fn move_abs(&mut self, x: i32, y: i32) -> Result<()> {
        let at = clamp(&self.displays, self.to_points(x, y));
        let ty = if self.buttons[0] { CGEventType::LeftMouseDragged } else if self.buttons[1] { CGEventType::RightMouseDragged } else { CGEventType::MouseMoved };
        self.mouse(ty, at, CGMouseButton::Left);
        Ok(())
    }

    fn move_rel(&mut self, dx: i16, dy: i16) -> Result<()> {
        let p = self.location();
        let d = &self.displays[0].1;
        let at = clamp(&self.displays, CGPoint { x: p.x + dx as f64 / d.scale as f64, y: p.y + dy as f64 / d.scale as f64 });
        let ty = if self.buttons[0] { CGEventType::LeftMouseDragged } else { CGEventType::MouseMoved };
        self.mouse(ty, at, CGMouseButton::Left);
        Ok(())
    }

    fn button(&mut self, button: u8, down: bool) -> Result<()> {
        let at = self.location();
        let (ty, b) = match (button, down) {
            (1, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
            (1, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
            (2, true) => (CGEventType::RightMouseDown, CGMouseButton::Right),
            (2, false) => (CGEventType::RightMouseUp, CGMouseButton::Right),
            (3, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center),
            (3, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center),
            _ => return Ok(()),
        };
        if (1..=3).contains(&button) { self.buttons[button as usize - 1] = down; }
        self.mouse(ty, at, b);
        Ok(())
    }

    fn wheel(&mut self, dx: i16, dy: i16) -> Result<()> {
        // Pixel units for smooth scrolling: 1/120 notch ≈ 0.3 px of a 40 px line.
        let e = CGEvent::new_scroll_wheel_event2(self.source.as_deref(), CGScrollEventUnit::Pixel, 2,
            (dy as i32 / 3).max(-1000), (dx as i32 / 3).max(-1000), 0);
        self.post(e);
        let _ = &mut self.wheel;
        Ok(())
    }

    fn key(&mut self, hid: u16, down: bool) -> Result<()> {
        if let Some(f) = modifier_flag(hid) { if down { self.flags |= f } else { self.flags &= !f } }
        let Some(vk) = hid_to_vk(hid) else { return Ok(()) };
        let e = CGEvent::new_keyboard_event(self.source.as_deref(), vk, down);
        if let Some(e) = &e { CGEvent::set_flags(Some(e), self.flags); }
        self.post(e);
        Ok(())
    }

    /// Text as Unicode key events, 20 UTF-16 units at a time (the limit CGEvent keeps).
    fn text(&mut self, s: &str) -> Result<()> {
        let units: Vec<u16> = s.encode_utf16().collect();
        for chunk in units.chunks(20) {
            for down in [true, false] {
                let e = CGEvent::new_keyboard_event(self.source.as_deref(), 0, down);
                if let Some(e) = &e {
                    unsafe { CGEvent::keyboard_set_unicode_string(Some(e), chunk.len() as _, chunk.as_ptr()) };
                }
                self.post(e);
            }
        }
        Ok(())
    }

    fn tap(&mut self, hids: &[u16]) -> Result<()> {
        for h in hids { self.key(*h, true)?; }
        for h in hids.iter().rev() { self.key(*h, false)?; }
        Ok(())
    }

    fn flush(&mut self) {}

    fn lock_screen(&mut self) { let _ = std::process::Command::new("pmset").arg("displaysleepnow").status(); }

    fn needs(&self) -> Vec<String> {
        let mut n = needs();
        if self.denied && !n.iter().any(|x| x == "screen-recording") { n.insert(0, "screen-recording".into()); }
        n
    }

    fn clip_set(&mut self, text: String) {
        use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
        unsafe {
            let pb = NSPasteboard::generalPasteboard();
            pb.clearContents();
            pb.setString_forType(&NSString::from_str(&text), NSPasteboardTypeString);
        }
    }

    fn clip_get(&mut self) -> Option<String> {
        use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
        unsafe { NSPasteboard::generalPasteboard().stringForType(NSPasteboardTypeString).map(|s| s.to_string()) }
    }

    fn action(&mut self, action: u8) -> Result<()> {
        // Show desktop: F11; all windows (Mission Control): Control + Up.
        match action {
            proto::action::SHOW_DESKTOP => self.tap(&[0x44]),
            proto::action::ALL_WINDOWS => self.tap(&[0xE0, 0x52]),
            _ => Ok(()),
        }
    }
}

/// H.264 through VideoToolbox: real time, no frame reordering, low-latency rate control, keyframes
/// only when asked. Frames go in as NV12, scaled with the same filter as the other platforms.
pub struct VtEncoder {
    session: Option<CFRetained<objc2_video_toolbox::VTCompressionSession>>,
    w: usize,
    h: usize,
    bitrate: u32,
    fps: f32,
    key: bool,
    out: Arc<Mutex<Option<(Vec<u8>, bool)>>>,
    yuv: Vec<u8>,
    /// Presentation times: the rate control paces by them, and drops frames that all claim 0.
    start: std::time::Instant,
}

impl VtEncoder {
    pub fn new() -> VtEncoder {
        VtEncoder { session: None, w: 0, h: 0, bitrate: 4_000_000, fps: 30.0, key: true, out: Arc::new(Mutex::new(None)), yuv: Vec::new(),
            start: std::time::Instant::now() }
    }

    fn set(&self, key: &CFString, value: &CFType) {
        if let Some(s) = &self.session {
            unsafe { objc2_video_toolbox::VTSessionSetProperty(s.as_ref() as &CFType, key, Some(value)) };
        }
    }

    fn make(&mut self, w: usize, h: usize) -> Result<()> {
        use objc2_video_toolbox::*;
        let spec = unsafe {
            CFDictionary::<CFString, CFBoolean>::from_slices(&[kVTVideoEncoderSpecification_EnableLowLatencyRateControl], &[CFBoolean::new(true)])
        };
        let mut session: *mut VTCompressionSession = std::ptr::null_mut();
        let status = unsafe {
            VTCompressionSession::create(None, w as i32, h as i32, u32::from_be_bytes(*b"avc1"), Some(spec.as_opaque()), None, None,
                None, std::ptr::null_mut(), NonNull::new(&mut session).unwrap())
        };
        if status != 0 || session.is_null() { return Err(format!("VTCompressionSessionCreate {status}")); }
        self.session = Some(unsafe { CFRetained::from_raw(NonNull::new(session).unwrap()) });
        unsafe {
            self.set(kVTCompressionPropertyKey_RealTime, CFBoolean::new(true));
            self.set(kVTCompressionPropertyKey_AllowFrameReordering, CFBoolean::new(false));
            self.set(kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_Main_AutoLevel);
            self.set(kVTCompressionPropertyKey_MaxKeyFrameInterval, &CFNumber::new_i32(i32::MAX));
            self.set(kVTCompressionPropertyKey_AverageBitRate, &CFNumber::new_i32(self.bitrate as i32));
            self.set(kVTCompressionPropertyKey_ExpectedFrameRate, &CFNumber::new_f64(self.fps as f64));
        }
        self.w = w;
        self.h = h;
        self.key = true;
        Ok(())
    }
}

impl VideoEncoder for VtEncoder {
    fn name(&self) -> &'static str { "videotoolbox" }

    fn set_bitrate(&mut self, bps: u32) {
        self.bitrate = bps;
        unsafe { self.set(objc2_video_toolbox::kVTCompressionPropertyKey_AverageBitRate, &CFNumber::new_i32(bps as i32)) };
    }

    fn set_fps(&mut self, fps: f32) {
        self.fps = fps;
        unsafe { self.set(objc2_video_toolbox::kVTCompressionPropertyKey_ExpectedFrameRate, &CFNumber::new_f64(fps as f64)) };
    }

    fn force_key(&mut self) { self.key = true; }

    fn encode(&mut self, bgra: &[u8], sw: usize, sh: usize, stride: usize, w: usize, h: usize) -> std::result::Result<(Vec<u8>, bool), String> {
        use objc2_video_toolbox::*;
        if self.session.is_none() || (w, h) != (self.w, self.h) { self.make(w, h)?; }
        to_i420(bgra, sw, sh, stride, w, h, &mut self.yuv);
        // I420 into an NV12 pixel buffer (what the hardware encoder takes).
        let mut pb: *mut CVPixelBuffer = std::ptr::null_mut();
        let st = unsafe { CVPixelBufferCreate(None, w, h, u32::from_be_bytes(*b"420v"), None, NonNull::new(&mut pb).unwrap()) };
        if st != 0 || pb.is_null() { return Err(format!("CVPixelBufferCreate {st}")); }
        let pb: CFRetained<CVPixelBuffer> = unsafe { CFRetained::from_raw(NonNull::new(pb).unwrap()) };
        unsafe {
            CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags::empty());
            let y = CVPixelBufferGetBaseAddressOfPlane(&pb, 0) as *mut u8;
            let ys = CVPixelBufferGetBytesPerRowOfPlane(&pb, 0);
            let uv = CVPixelBufferGetBaseAddressOfPlane(&pb, 1) as *mut u8;
            let uvs = CVPixelBufferGetBytesPerRowOfPlane(&pb, 1);
            for r in 0..h { std::ptr::copy_nonoverlapping(self.yuv.as_ptr().add(r * w), y.add(r * ys), w); }
            let (u_plane, v_plane) = (w * h, w * h + w * h / 4);
            for r in 0..h / 2 {
                let row = uv.add(r * uvs);
                for c in 0..w / 2 {
                    *row.add(2 * c) = self.yuv[u_plane + r * (w / 2) + c];
                    *row.add(2 * c + 1) = self.yuv[v_plane + r * (w / 2) + c];
                }
            }
            CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags::empty());
        }
        let props = if self.key {
            Some(unsafe { CFDictionary::<CFString, CFBoolean>::from_slices(&[kVTEncodeFrameOptionKey_ForceKeyFrame], &[CFBoolean::new(true)]) })
        } else { None };
        self.key = false;
        let out = self.out.clone();
        let handler = RcBlock::new(move |status: i32, flags: VTEncodeInfoFlags, sample: *mut CMSampleBuffer| {
            if status != 0 { return; }
            // The low-latency rate control drops frames to hold the bitrate: nothing to send, no error.
            if flags.contains(VTEncodeInfoFlags::FrameDropped) { *out.lock().unwrap() = Some((Vec::new(), false)); return; }
            if let Some(s) = unsafe { sample.as_ref() } { *out.lock().unwrap() = annex_b(s); }
        });
        let session = self.session.as_ref().unwrap();
        let pts = CMTime { value: self.start.elapsed().as_micros() as i64, timescale: 1_000_000, flags: CMTimeFlags::Valid, epoch: 0 };
        let st = unsafe {
            session.encode_frame_with_output_handler(&pb, pts,
                objc2_core_media::kCMTimeInvalid, props.as_ref().map(|p| p.as_opaque()), std::ptr::null_mut(), &*handler as *const _ as *mut _)
        };
        if st != 0 { return Err(format!("VTCompressionSessionEncodeFrame {st}")); }
        unsafe { session.complete_frames(objc2_core_media::kCMTimeInvalid) };
        self.out.lock().unwrap().take().ok_or_else(|| "no output".to_string())
    }
}

/// An AVCC sample (length-prefixed NAL units) as Annex-B, with SPS and PPS before keyframes.
fn annex_b(s: &CMSampleBuffer) -> Option<(Vec<u8>, bool)> {
    use objc2_core_media::*;
    let mut out = Vec::new();
    let attachments = unsafe { s.sample_attachments_array(false) };
    let key = attachments.map_or(true, |a| unsafe {
        let a: &objc2_core_foundation::CFArray = &a;
        if a.count() == 0 { return true; }
        let d = a.value_at_index(0) as *const CFDictionary;
        !(*d).contains_ptr_key(kCMSampleAttachmentKey_NotSync as *const _ as *const _)
    });
    if key {
        let fd = unsafe { s.format_description() }?;
        let mut count = 0usize;
        unsafe { CMVideoFormatDescriptionGetH264ParameterSetAtIndex(&fd, 0, std::ptr::null_mut(), std::ptr::null_mut(), &mut count, std::ptr::null_mut()) };
        for i in 0..count {
            let mut p: *const u8 = std::ptr::null();
            let mut n = 0usize;
            if unsafe { CMVideoFormatDescriptionGetH264ParameterSetAtIndex(&fd, i, &mut p, &mut n, std::ptr::null_mut(), std::ptr::null_mut()) } == 0 {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(unsafe { std::slice::from_raw_parts(p, n) });
            }
        }
    }
    let block = unsafe { s.data_buffer() }?;
    let len = unsafe { block.data_length() };
    let mut data = vec![0u8; len];
    if unsafe { block.copy_data_bytes(0, len, NonNull::new(data.as_mut_ptr() as *mut _).unwrap()) } != 0 { return None; }
    let mut i = 0;
    while i + 4 <= data.len() {
        let n = u32::from_be_bytes(data[i..i + 4].try_into().unwrap()) as usize;
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[i + 4..(i + 4 + n).min(data.len())]);
        i += 4 + n;
    }
    Some((out, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// VideoToolbox needs no permission: frames of a moving gradient come out as Annex-B with
    /// SPS, PPS and an IDR first, then P-frames, and a forced keyframe when asked.
    #[test]
    fn videotoolbox_encodes_annex_b() {
        let (w, h) = (640usize, 360usize);
        let mut enc = VtEncoder::new();
        enc.set_bitrate(2_000_000);
        let mut all = Vec::new();
        let mut keys = Vec::new();
        for f in 0..10usize {
            let bgra: Vec<u8> = (0..w * h).flat_map(|i| { let x = (i % w + f * 8) as u8; [x, (i / w) as u8, 128, 255] }).collect();
            if f == 6 { enc.force_key(); }
            let (data, key) = enc.encode(&bgra, w, h, w * 4, w, h).expect("encode");
            assert!(data.starts_with(&[0, 0, 0, 1]), "frame {f} is Annex-B");
            keys.push(key);
            all.extend(data);
        }
        assert!(keys[0] && !keys[1] && keys[6], "keys {keys:?}");
        let types: Vec<u8> = all.windows(5).filter(|w| w[..4] == [0, 0, 0, 1]).map(|w| w[4] & 0x1f).collect();
        assert!(types.contains(&7) && types.contains(&8) && types.contains(&5), "nal types {types:?}");
        std::fs::write(std::env::temp_dir().join("roamtty-vt.h264"), &all).unwrap();
    }

    #[test]
    fn moves_stay_on_a_display() {
        let d = |id: u8, x: i32| (id as u32, Display { id, w: 3024, h: 1964, scale: 2.0, x, y: 0, primary: id == 0, name: String::new() });
        let two = [d(0, 0), d(1, 1512)];
        let at = |x: f64, y: f64| { let p = clamp(&two, CGPoint { x, y }); (p.x, p.y) };
        assert_eq!(at(100.0, 100.0), (100.0, 100.0));
        assert_eq!(at(2000.0, 500.0), (2000.0, 500.0), "the second display");
        assert_eq!(at(3500.0, 2000.0), (3023.0, 981.0), "past the right edge");
        assert_eq!(at(-40.0, -9.0), (0.0, 0.0));
    }

    /// A full screen of noise at the session's pace is more than 4 Mbps holds: the frames the rate
    /// control drops come back empty, not as errors, and the rest still decode as a stream.
    #[test]
    fn videotoolbox_keeps_every_frame_of_a_busy_screen() {
        let (w, h) = (1920usize, 1246usize);
        let mut enc = VtEncoder::new();
        enc.set_bitrate(4_000_000);
        let mut seed = 1u32;
        let (mut sent, mut dropped) = (0, 0);
        for f in 0..90usize {
            let bgra: Vec<u8> = (0..w * h * 4).map(|_| { seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345); (seed >> 24) as u8 }).collect();
            if f % 30 == 15 { enc.set_bitrate(1_000_000 + f as u32 * 50_000); }
            let (data, _) = enc.encode(&bgra, w, h, w * 4, w, h).expect("a dropped frame is not an error");
            if data.is_empty() { dropped += 1 } else { sent += 1 }
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
        assert!(sent > 0 && dropped > 0, "sent {sent} dropped {dropped}");
    }

    /// Needs the GUI session: over SSH there is no WindowServer, which is why the agent exists.
    #[test]
    fn displays_are_listed() {
        let Ok(m) = Mac::open() else { eprintln!("no WindowServer here (SSH): skipped"); return };
        let d = m.displays();
        assert!(!d.is_empty());
        assert!(d.iter().any(|x| x.primary));
        assert!(d[0].w >= 640 && d[0].scale >= 1.0, "{:?}", d[0]);
        println!("displays {d:?} needs {:?}", needs());
    }

    #[test]
    fn keys_map_by_position() {
        assert_eq!(hid_to_vk(0x04), Some(0x00)); // A
        assert_eq!(hid_to_vk(0x1D), Some(0x06)); // Z
        assert_eq!(hid_to_vk(0x28), Some(0x24)); // Return
        assert_eq!(hid_to_vk(0xE3), Some(0x37)); // Command
    }
}
