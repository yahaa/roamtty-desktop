//! The host's sound for the phone: whatever plays on the default output, taken where each system
//! offers it (the default sink's monitor through PulseAudio or PipeWire on Linux, WASAPI loopback on
//! Windows, ScreenCaptureKit on macOS) and sent as 20 ms Opus packets of 48 kHz stereo.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

pub const RATE: u32 = 48_000;
/// Samples per channel in one packet: 20 ms.
pub const FRAME: usize = 960;
const BITRATE: i32 = 96_000;
/// Silent packets in a row that still go out (so the phone's decoder fades out); after them, none until sound returns.
const SILENT_TAIL: u32 = 10;

/// 48 kHz stereo, interleaved i16.
pub trait Source {
    /// Appends what was captured since the last call, waiting up to about 100 ms for some.
    fn read(&mut self, out: &mut Vec<i16>) -> Result<(), String>;
}

/// Capturing while it lives; dropping it stops the capture thread.
pub struct Audio { stop: Arc<AtomicBool>, thread: Option<std::thread::JoinHandle<()>> }

impl Drop for Audio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() { let _ = t.join(); }
    }
}

/// `on_packet(seq, capture_us, opus)` runs on the capture thread for every packet. Fails when the
/// system has no sound to capture (no audio server, no output device).
pub fn start(on_packet: impl Fn(u32, u64, Vec<u8>) + Send + 'static) -> Result<Audio, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let (opened, ready) = mpsc::channel();
    let flag = stop.clone();
    // The source is opened on its own thread: WASAPI's COM objects belong to the thread that made them.
    let thread = std::thread::Builder::new().name("audio".into()).spawn(move || {
        let mut enc = match Opus::new() {
            Ok(e) => e,
            Err(e) => { let _ = opened.send(Err(e)); return; }
        };
        let mut src = match open_source() {
            Ok(s) => { let _ = opened.send(Ok(())); s }
            Err(e) => { let _ = opened.send(Err(e)); return; }
        };
        let mut pcm: Vec<i16> = Vec::with_capacity(FRAME * 8);
        let mut seq = 0u32;
        let mut silent = 0u32;
        let mut sent = 0u64;
        while !flag.load(Ordering::SeqCst) {
            if let Err(e) = src.read(&mut pcm) {
                // The output device changed or went away: open the new default one.
                crate::log!("audio read failed: {e}");
                drop(src);
                loop {
                    if flag.load(Ordering::SeqCst) { return; }
                    std::thread::sleep(Duration::from_secs(1));
                    if let Ok(s) = open_source() { src = s; break; }
                }
                pcm.clear();
                continue;
            }
            let now = crate::serve::now_us();
            let frames = pcm.len() / (FRAME * 2);
            for (i, f) in pcm.chunks_exact(FRAME * 2).enumerate() {
                seq = seq.wrapping_add(1);
                silent = if f.iter().all(|s| s.unsigned_abs() <= 2) { silent.saturating_add(1) } else { 0 };
                if silent > SILENT_TAIL { continue; }
                match enc.encode(f) {
                    Ok(p) => {
                        // The newest sample was captured about now; earlier packets of this read before it.
                        let age = (frames - 1 - i) as u64 * 20_000;
                        on_packet(seq, now.saturating_sub(age), p);
                        sent += 1;
                    }
                    Err(e) => crate::log!("opus: {e}"),
                }
            }
            pcm.drain(..frames * FRAME * 2);
        }
        crate::log!("audio end packets={sent}");
    }).map_err(|e| e.to_string())?;
    match ready.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(Audio { stop, thread: Some(thread) }),
        Ok(Err(e)) => { let _ = thread.join(); Err(e) }
        Err(_) => { stop.store(true, Ordering::SeqCst); Err("audio did not start".into()) }
    }
}

struct Opus(*mut audiopus_sys::OpusEncoder);

unsafe impl Send for Opus {}

impl Opus {
    fn new() -> Result<Opus, String> {
        use audiopus_sys::*;
        let mut err = 0;
        let e = unsafe { opus_encoder_create(RATE as i32, 2, OPUS_APPLICATION_AUDIO, &mut err) };
        if e.is_null() || err != OPUS_OK { return Err(format!("opus_encoder_create {err}")); }
        unsafe {
            opus_encoder_ctl(e, OPUS_SET_BITRATE_REQUEST, BITRATE);
            // Music and speech alike; the CELT side keeps the delay at 6.5 ms beyond the frame.
            opus_encoder_ctl(e, OPUS_SET_SIGNAL_REQUEST, OPUS_SIGNAL_MUSIC);
        }
        Ok(Opus(e))
    }

    fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>, String> {
        let mut out = vec![0u8; 1500];
        let n = unsafe { audiopus_sys::opus_encode(self.0, pcm.as_ptr(), FRAME as i32, out.as_mut_ptr(), out.len() as i32) };
        if n < 0 { return Err(format!("opus_encode {n}")); }
        out.truncate(n as usize);
        Ok(out)
    }
}

impl Drop for Opus {
    fn drop(&mut self) { unsafe { audiopus_sys::opus_encoder_destroy(self.0) } }
}

/// Float samples to 48 kHz stereo i16: `planes` holds one slice per channel, or one interleaved slice
/// of `channels`. Other rates are resampled linearly, which is plenty for a phone's speaker.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub fn push_float(out: &mut Vec<i16>, planes: &[&[f32]], channels: usize, rate: u32, phase: &mut f64) {
    let channels = channels.max(1);
    let frames = if planes.len() > 1 { planes[0].len() } else { planes.first().map_or(0, |p| p.len() / channels) };
    let at = |frame: usize, ch: usize| -> f32 {
        let ch = ch.min(channels - 1);
        if planes.len() > 1 { planes[ch.min(planes.len() - 1)][frame] } else { planes[0][frame * channels + ch] }
    };
    let to_i16 = |v: f32| (v.clamp(-1.0, 1.0) * 32767.0) as i16;
    if rate == RATE {
        for f in 0..frames {
            out.push(to_i16(at(f, 0)));
            out.push(to_i16(at(f, 1)));
        }
        return;
    }
    let step = rate as f64 / RATE as f64;
    while (*phase as usize) + 1 < frames {
        let i = *phase as usize;
        let t = (*phase - i as f64) as f32;
        for ch in 0..2 { out.push(to_i16(at(i, ch) * (1.0 - t) + at(i + 1, ch) * t)); }
        *phase += step;
    }
    // Carry the fraction into the next buffer; its last sample is dropped, not interpolated across.
    *phase -= (frames.saturating_sub(1)) as f64;
    if *phase < 0.0 { *phase = 0.0; }
}

#[cfg(target_os = "linux")]
fn open_source() -> Result<Box<dyn Source>, String> { pulse::Pulse::open().map(|p| Box::new(p) as Box<dyn Source>) }

#[cfg(windows)]
fn open_source() -> Result<Box<dyn Source>, String> { wasapi::Loopback::open().map(|p| Box::new(p) as Box<dyn Source>) }

#[cfg(target_os = "macos")]
fn open_source() -> Result<Box<dyn Source>, String> { crate::mac::AudioCapture::open().map(|p| Box::new(p) as Box<dyn Source>) }

/// `audio-test`: captures for `seconds` and prints the packets and bytes it made. Silence stops the
/// packets after 200 ms, so a count near 50 a second means something played.
pub fn self_test(seconds: u64) -> i32 {
    let count = Arc::new(std::sync::Mutex::new((0u64, 0usize)));
    let p = count.clone();
    let audio = start(move |_, _, opus| { let mut p = p.lock().unwrap(); p.0 += 1; p.1 += opus.len(); });
    let audio = match audio {
        Ok(a) => a,
        Err(e) => { println!("{}", serde_json::json!({ "ok": false, "error": e })); return 1; }
    };
    std::thread::sleep(Duration::from_secs(seconds));
    drop(audio);
    let (packets, bytes) = *count.lock().unwrap();
    println!("{}", serde_json::json!({ "ok": true, "packets": packets, "bytes": bytes }));
    0
}

/// PulseAudio's simple API, loaded at run time so a host without it still serves the picture.
/// PipeWire answers it through pipewire-pulse.
#[cfg(target_os = "linux")]
mod pulse {
    use std::ffi::{c_char, c_int, c_void, CStr};

    #[repr(C)]
    struct SampleSpec { format: c_int, rate: u32, channels: u8 }
    #[repr(C)]
    struct BufferAttr { maxlength: u32, tlength: u32, prebuf: u32, minreq: u32, fragsize: u32 }

    const PA_SAMPLE_S16LE: c_int = 3;
    const PA_STREAM_RECORD: c_int = 2;

    type New = unsafe extern "C" fn(*const c_char, *const c_char, c_int, *const c_char, *const c_char,
        *const SampleSpec, *const c_void, *const BufferAttr, *mut c_int) -> *mut c_void;
    type Read = unsafe extern "C" fn(*mut c_void, *mut c_void, usize, *mut c_int) -> c_int;
    type Free = unsafe extern "C" fn(*mut c_void);
    type StrError = unsafe extern "C" fn(c_int) -> *const c_char;

    pub struct Pulse { _lib: libloading::Library, s: *mut c_void, read: Read, free: Free, strerror: StrError }

    impl Pulse {
        pub fn open() -> Result<Pulse, String> {
            let lib = unsafe { libloading::Library::new("libpulse-simple.so.0") }.map_err(|_| "no PulseAudio client library".to_string())?;
            let (new, read, free, strerror) = unsafe {
                let sym = |n: &[u8]| -> Result<*const c_void, String> {
                    lib.get::<*const c_void>(n).map(|s| *s).map_err(|e| e.to_string())
                };
                (std::mem::transmute::<*const c_void, New>(sym(b"pa_simple_new\0")?),
                    std::mem::transmute::<*const c_void, Read>(sym(b"pa_simple_read\0")?),
                    std::mem::transmute::<*const c_void, Free>(sym(b"pa_simple_free\0")?),
                    std::mem::transmute::<*const c_void, StrError>(sym(b"pa_strerror\0")?))
            };
            let spec = SampleSpec { format: PA_SAMPLE_S16LE, rate: super::RATE, channels: 2 };
            // 10 ms fragments: what the server hands over as soon as it has it.
            let attr = BufferAttr { maxlength: u32::MAX, tlength: u32::MAX, prebuf: u32::MAX, minreq: u32::MAX, fragsize: 1920 };
            // An SSH login may lack XDG_RUNTIME_DIR, where the client looks for the user's server.
            let server = (std::env::var_os("PULSE_SERVER").is_none() && std::env::var_os("XDG_RUNTIME_DIR").is_none())
                .then(|| format!("unix:/run/user/{}/pulse/native\0", unsafe { libc::getuid() }));
            let mut err = 0;
            let s = unsafe {
                new(server.as_ref().map_or(std::ptr::null(), |s| s.as_ptr() as *const c_char), c"Roamtty".as_ptr(), PA_STREAM_RECORD,
                    c"@DEFAULT_MONITOR@".as_ptr(), c"desktop sound".as_ptr(), &spec, std::ptr::null(), &attr, &mut err)
            };
            if s.is_null() { return Err(format!("PulseAudio: {}", unsafe { CStr::from_ptr(strerror(err)) }.to_string_lossy())); }
            Ok(Pulse { _lib: lib, s, read, free, strerror })
        }
    }

    impl super::Source for Pulse {
        fn read(&mut self, out: &mut Vec<i16>) -> Result<(), String> {
            let mut buf = [0i16; super::FRAME * 2];
            let mut err = 0;
            if unsafe { (self.read)(self.s, buf.as_mut_ptr() as *mut c_void, std::mem::size_of_val(&buf), &mut err) } < 0 {
                return Err(unsafe { CStr::from_ptr((self.strerror)(err)) }.to_string_lossy().into_owned());
            }
            out.extend_from_slice(&buf);
            Ok(())
        }
    }

    impl Drop for Pulse {
        fn drop(&mut self) { unsafe { (self.free)(self.s) } }
    }
}

/// WASAPI loopback of the default output. Nothing comes while nothing plays, so silence costs nothing.
#[cfg(windows)]
mod wasapi {
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::*;

    pub struct Loopback { client: IAudioClient, capture: IAudioCaptureClient, channels: usize, rate: u32, float: bool, phase: f64 }

    impl Loopback {
        pub fn open() -> Result<Loopback, String> {
            let e = |e: windows::core::Error| e.message();
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(e)?;
                let device = devices.GetDefaultAudioEndpoint(eRender, eConsole).map_err(|_| "no audio output".to_string())?;
                let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(e)?;
                // 48 kHz stereo i16 converted by the audio engine; else the mix format as it is.
                let want = WAVEFORMATEX { wFormatTag: WAVE_FORMAT_PCM as u16, nChannels: 2, nSamplesPerSec: super::RATE,
                    nAvgBytesPerSec: super::RATE * 4, nBlockAlign: 4, wBitsPerSample: 16, cbSize: 0 };
                let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
                let (channels, rate, float) = if client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, &want, None).is_ok() {
                    (2, super::RATE, false)
                } else {
                    let client2: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(e)?;
                    let mix = client2.GetMixFormat().map_err(e)?;
                    let f = *mix;
                    let (bits, channels, rate) = (f.wBitsPerSample, f.nChannels, f.nSamplesPerSec);
                    // The shared mix format is 32-bit float (WAVE_FORMAT_EXTENSIBLE, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT).
                    if bits != 32 { CoTaskMemFree(Some(mix as _)); return Err(format!("mix format {bits} bit")); }
                    let r = client2.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, 2_000_000, 0, mix, None);
                    CoTaskMemFree(Some(mix as _));
                    r.map_err(e)?;
                    let capture: IAudioCaptureClient = client2.GetService().map_err(e)?;
                    client2.Start().map_err(e)?;
                    return Ok(Loopback { client: client2, capture, channels: channels as usize, rate, float: true, phase: 0.0 });
                };
                let capture: IAudioCaptureClient = client.GetService().map_err(e)?;
                client.Start().map_err(e)?;
                Ok(Loopback { client, capture, channels, rate, float, phase: 0.0 })
            }
        }
    }

    impl super::Source for Loopback {
        fn read(&mut self, out: &mut Vec<i16>) -> Result<(), String> {
            std::thread::sleep(std::time::Duration::from_millis(10));
            unsafe {
                loop {
                    let n = self.capture.GetNextPacketSize().map_err(|e| e.message())?;
                    if n == 0 { return Ok(()); }
                    let mut data = std::ptr::null_mut();
                    let mut frames = 0u32;
                    let mut flags = 0u32;
                    self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None).map_err(|e| e.message())?;
                    let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                    if !self.float {
                        if silent { out.resize(out.len() + frames as usize * 2, 0); }
                        else { out.extend_from_slice(std::slice::from_raw_parts(data as *const i16, frames as usize * 2)); }
                    } else {
                        let n = frames as usize * self.channels;
                        let zeros;
                        let samples: &[f32] = if silent { zeros = vec![0f32; n]; &zeros } else { std::slice::from_raw_parts(data as *const f32, n) };
                        super::push_float(out, &[samples], self.channels, self.rate, &mut self.phase);
                    }
                    self.capture.ReleaseBuffer(frames).map_err(|e| e.message())?;
                }
            }
        }
    }

    impl Drop for Loopback {
        fn drop(&mut self) { unsafe { let _ = self.client.Stop(); } }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_makes_packets() {
        let mut e = Opus::new().unwrap();
        let tone: Vec<i16> = (0..FRAME * 2).map(|i| (((i / 2) as f32 * 440.0 * std::f32::consts::TAU / RATE as f32).sin() * 8000.0) as i16).collect();
        let p = e.encode(&tone).unwrap();
        assert!(!p.is_empty() && p.len() < 1500);
    }

    #[test]
    fn floats_become_stereo_48k() {
        let mut out = Vec::new();
        let mut phase = 0.0;
        push_float(&mut out, &[&[0.5, -0.5], &[1.0, 2.0]], 2, RATE, &mut phase);
        assert_eq!(out, vec![16383, 32767, -16383, 32767]);
        // Mono is copied to both sides; 44.1 kHz comes out at 48 kHz.
        out.clear();
        let mono = vec![0.25f32; 4410];
        push_float(&mut out, &[&mono], 1, 44_100, &mut phase);
        assert!((out.len() / 2).abs_diff(4800) <= 2, "{}", out.len());
        assert!(out.iter().all(|s| *s == 8191));
    }
}
