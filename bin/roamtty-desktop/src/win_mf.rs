//! H.264 on the GPU through Media Foundation's hardware encoders (NVENC, Quick Sync, AMF): the
//! duplicated desktop texture is scaled and converted to NV12 by the D3D11 video processor and
//! handed to the encoder as a texture, so no pixel goes through the CPU. Hardware MFTs are
//! asynchronous: they ask for input and announce output with events. Without one (a VM, Remote
//! Desktop's adapter) `serve` keeps using OpenH264 on the CPU.

use crate::encode::VideoEncoder;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};
use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{VARIANT, VT_UI4};

fn err(e: impl std::fmt::Display) -> String { e.to_string() }

/// The newest desktop picture, kept on the GPU: the backend copies each new frame in, the encoder
/// reads it. Both live on the serve thread.
pub struct GpuFrame {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub texture: RefCell<Option<(ID3D11Texture2D, u32, u32)>>,
}

impl GpuFrame {
    /// Copies a duplicated frame into the shared texture (made again when the size changes).
    pub fn store(&self, src: &ID3D11Texture2D, w: u32, h: u32) -> Result<(), String> {
        let mut t = self.texture.borrow_mut();
        if t.as_ref().is_none_or(|(_, tw, th)| (*tw, *th) != (w, h)) {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w, Height: h, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32, CPUAccessFlags: 0, MiscFlags: 0,
            };
            let mut tex = None;
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex)) }.map_err(err)?;
            *t = Some((tex.ok_or("no texture")?, w, h));
        }
        unsafe { self.context.CopyResource(&t.as_ref().unwrap().0, src) };
        Ok(())
    }
}

/// Scales and converts BGRA to NV12 on the GPU, into a few textures in turn: the encoder may still
/// be reading the last one while the next frame is drawn.
struct Converter {
    video: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    out: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    input: Option<(usize, ID3D11VideoProcessorInputView)>,
    size: (u32, u32, u32, u32),
}

impl Converter {
    fn new(gpu: &GpuFrame, sw: u32, sh: u32, w: u32, h: u32) -> Result<Converter, String> {
        let video: ID3D11VideoDevice = gpu.device.cast().map_err(err)?;
        let vctx: ID3D11VideoContext = gpu.context.cast().map_err(err)?;
        let rate = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, InputFrameRate: rate, InputWidth: sw, InputHeight: sh,
            OutputFrameRate: rate, OutputWidth: w, OutputHeight: h, Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let enumerator = unsafe { video.CreateVideoProcessorEnumerator(&desc) }.map_err(err)?;
        let processor = unsafe { video.CreateVideoProcessor(&enumerator, 0) }.map_err(err)?;
        unsafe {
            // Desktop RGB is full range; out goes BT.601 limited range, what decoders assume when
            // the stream says nothing (as OpenH264's does).
            vctx.VideoProcessorSetStreamColorSpace(&processor, 0, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 });
            vctx.VideoProcessorSetOutputColorSpace(&processor, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 1 << 4 });
            vctx.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vctx.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            let src = RECT { left: 0, top: 0, right: sw as i32, bottom: sh as i32 };
            let dst = RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
            vctx.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src));
            vctx.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst));
            vctx.VideoProcessorSetOutputTargetRect(&processor, true, Some(&dst));
        }
        let mut out = Vec::new();
        for _ in 0..3 {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w, Height: h, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32, CPUAccessFlags: 0, MiscFlags: 0,
            };
            let mut tex = None;
            unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut tex)) }.map_err(err)?;
            let tex = tex.ok_or("no NV12 texture")?;
            let vd = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC { ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D, ..Default::default() };
            let mut view = None;
            unsafe { video.CreateVideoProcessorOutputView(&tex, &enumerator, &vd, Some(&mut view)) }.map_err(err)?;
            out.push((tex, view.ok_or("no output view")?));
        }
        Ok(Converter { video, vctx, enumerator, processor, out, next: 0, input: None, size: (sw, sh, w, h) })
    }

    /// Draws `src` into the next NV12 texture and returns it.
    fn convert(&mut self, src: &ID3D11Texture2D) -> Result<ID3D11Texture2D, String> {
        let key = src.as_raw() as usize;
        if self.input.as_ref().is_none_or(|(k, _)| *k != key) {
            let vd = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC { FourCC: 0, ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D, ..Default::default() };
            let mut view = None;
            unsafe { self.video.CreateVideoProcessorInputView(src, &self.enumerator, &vd, Some(&mut view)) }.map_err(err)?;
            self.input = Some((key, view.ok_or("no input view")?));
        }
        let (tex, view) = self.out[self.next].clone();
        self.next = (self.next + 1) % self.out.len();
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(self.input.as_ref().unwrap().1.clone())),
            ..Default::default()
        };
        let streams = [stream];
        let r = unsafe { self.vctx.VideoProcessorBlt(&self.processor, &view, 0, &streams) };
        let [mut stream] = streams;
        unsafe { std::mem::ManuallyDrop::drop(&mut stream.pInputSurface) };
        r.map_err(err)?;
        Ok(tex)
    }
}

fn u4(v: u32) -> VARIANT {
    let mut var = VARIANT::default();
    unsafe {
        let inner = &mut *var.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = v;
    }
    var
}

/// The encoder for one size: the MFT, its event queue and its settings interface.
struct Session {
    activate: IMFActivate,
    mft: IMFTransform,
    events: IMFMediaEventGenerator,
    api: Option<ICodecAPI>,
    /// METransformNeedInput events not yet answered with a frame.
    need_input: u32,
    provides_samples: bool,
    /// SPS + PPS in Annex-B, put before a keyframe that lacks them.
    headers: Vec<u8>,
    w: u32,
    h: u32,
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = self.activate.ShutdownObject();
        }
    }
}

pub struct MfEncoder {
    gpu: Rc<GpuFrame>,
    manager: IMFDXGIDeviceManager,
    activate: IMFActivate,
    name: &'static str,
    session: Option<Session>,
    converter: Option<Converter>,
    bitrate: u32,
    fps: f32,
    key: bool,
    start: Instant,
}

/// The hardware H.264 encoders on the adapter of `device`, best first.
fn hardware_encoders(device: &ID3D11Device) -> Result<Vec<IMFActivate>, String> {
    let dxgi: IDXGIDevice = device.cast().map_err(err)?;
    let luid = unsafe { dxgi.GetAdapter().and_then(|a| a.GetDesc()) }.map_err(err)?.AdapterLuid;
    let mut attrs = None;
    unsafe { MFCreateAttributes(&mut attrs, 1) }.map_err(err)?;
    let attrs = attrs.ok_or("no attributes")?;
    let luid_bytes = unsafe { std::slice::from_raw_parts(&luid as *const _ as *const u8, std::mem::size_of_val(&luid)) };
    unsafe { attrs.SetBlob(&MFT_ENUM_ADAPTER_LUID, luid_bytes) }.map_err(err)?;
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut n = 0u32;
    unsafe {
        MFTEnum2(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER, Some(&input), Some(&output),
            &attrs, &mut list, &mut n).map_err(err)?;
    }
    let mut out = Vec::new();
    if !list.is_null() {
        for i in 0..n as usize {
            if let Some(a) = unsafe { (*list.add(i)).take() } { out.push(a); }
        }
        unsafe { CoTaskMemFree(Some(list as *const _)) };
    }
    Ok(out)
}

/// A short name for the stats from the MFT's vendor: nvenc, quicksync, amf.
fn vendor_name(a: &IMFActivate) -> &'static str {
    let mut buf = [0u16; 64];
    let mut len = 0u32;
    let vendor = unsafe { a.GetString(&MFT_ENUM_HARDWARE_VENDOR_ID_Attribute, &mut buf, Some(&mut len)) }
        .map(|_| String::from_utf16_lossy(&buf[..len as usize]).to_uppercase()).unwrap_or_default();
    if vendor.contains("10DE") { "nvenc" } else if vendor.contains("8086") { "quicksync" } else if vendor.contains("1002") { "amf" } else { "mf-hardware" }
}

impl MfEncoder {
    /// The first hardware encoder that takes D3D11 textures; Err when this machine has none.
    pub fn new(gpu: Rc<GpuFrame>) -> Result<MfEncoder, String> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(err)?;
            // The encoder's own threads use the device too.
            if let Ok(mt) = gpu.device.cast::<ID3D11Multithread>() { let _ = mt.SetMultithreadProtected(true); }
        }
        let mut token = 0u32;
        let mut manager = None;
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }.map_err(err)?;
        let manager = manager.ok_or("no device manager")?;
        unsafe { manager.ResetDevice(&gpu.device, token) }.map_err(err)?;
        let encoders = hardware_encoders(&gpu.device)?;
        let mut last = "no hardware H.264 encoder".to_string();
        for activate in encoders {
            let name = vendor_name(&activate);
            let mut enc = MfEncoder { gpu: gpu.clone(), manager: manager.clone(), activate, name, session: None, converter: None,
                bitrate: 4_000_000, fps: 60.0, key: true, start: Instant::now() };
            // Opened once at a small size to see that it works here at all.
            match enc.open(640, 360) {
                Ok(s) => { drop(s); return Ok(enc); }
                Err(e) => { crate::log!("hardware encoder {name}: {e}"); last = e; let _ = unsafe { enc.activate.ShutdownObject() }; }
            }
        }
        Err(last)
    }

    fn open(&mut self, w: u32, h: u32) -> Result<Session, String> {
        let mft: IMFTransform = unsafe { self.activate.ActivateObject() }.map_err(err)?;
        let attrs = unsafe { mft.GetAttributes() }.map_err(err)?;
        unsafe {
            if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 0 { return Err("not an asynchronous MFT".into()); }
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).map_err(err)?;
            if attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 { return Err("not D3D11 aware".into()); }
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, self.manager.as_raw() as usize).map_err(err)?;
        }
        let api = mft.cast::<ICodecAPI>().ok();
        if let Some(api) = &api {
            unsafe {
                // Low-delay VBR where the encoder has it, else CBR; no B-frames; keyframes on request.
                if api.SetValue(&CODECAPI_AVEncCommonRateControlMode, &u4(eAVEncCommonRateControlMode_LowDelayVBR.0 as u32)).is_err() {
                    let _ = api.SetValue(&CODECAPI_AVEncCommonRateControlMode, &u4(eAVEncCommonRateControlMode_CBR.0 as u32));
                }
                let _ = api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &u4(self.bitrate));
                let _ = api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &u4(0));
                let _ = api.SetValue(&CODECAPI_AVEncMPVGOPSize, &u4(3600));
            }
        }
        let fps = self.fps.round().max(1.0) as u64;
        let size = ((w as u64) << 32) | h as u64;
        let rate = (fps << 32) | 1;
        unsafe {
            let out = MFCreateMediaType().map_err(err)?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(err)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).map_err(err)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, self.bitrate).map_err(err)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, size).map_err(err)?;
            out.SetUINT64(&MF_MT_FRAME_RATE, rate).map_err(err)?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(err)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32).map_err(err)?;
            mft.SetOutputType(0, &out, 0).map_err(|e| format!("output type: {e}"))?;
            let input = MFCreateMediaType().map_err(err)?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(err)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).map_err(err)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, size).map_err(err)?;
            input.SetUINT64(&MF_MT_FRAME_RATE, rate).map_err(err)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(err)?;
            mft.SetInputType(0, &input, 0).map_err(|e| format!("input type: {e}"))?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(err)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(err)?;
        }
        let info = unsafe { mft.GetOutputStreamInfo(0) }.map_err(err)?;
        let provides_samples = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32) != 0;
        let headers = unsafe { mft.GetOutputCurrentType(0) }.ok().and_then(|t| sequence_header(&t)).unwrap_or_default();
        let events: IMFMediaEventGenerator = mft.cast().map_err(err)?;
        Ok(Session { activate: self.activate.clone(), mft, events, api, need_input: 0, provides_samples, headers, w, h })
    }

    /// Waits for the next event until `deadline`; None on timeout.
    fn next_event(s: &Session, deadline: Instant) -> Result<Option<u32>, String> {
        loop {
            match unsafe { s.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(e) => return Ok(Some(unsafe { e.GetType() }.map_err(err)?)),
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if Instant::now() >= deadline { return Ok(None); }
                    std::thread::sleep(Duration::from_micros(300));
                }
                Err(e) => return Err(err(e)),
            }
        }
    }

    fn take_output(s: &mut Session) -> Result<Option<(Vec<u8>, bool)>, String> {
        let mut buf = [MFT_OUTPUT_DATA_BUFFER::default()];
        if !s.provides_samples {
            let info = unsafe { s.mft.GetOutputStreamInfo(0) }.map_err(err)?;
            let sample = unsafe { MFCreateSample() }.map_err(err)?;
            let mb = unsafe { MFCreateMemoryBuffer(info.cbSize.max(1 << 20)) }.map_err(err)?;
            unsafe { sample.AddBuffer(&mb) }.map_err(err)?;
            buf[0].pSample = std::mem::ManuallyDrop::new(Some(sample));
        }
        let mut status = 0u32;
        let r = unsafe { s.mft.ProcessOutput(0, &mut buf, &mut status) };
        let sample = unsafe { std::mem::ManuallyDrop::take(&mut buf[0].pSample) };
        unsafe { drop(std::mem::ManuallyDrop::take(&mut buf[0].pEvents)) };
        match r {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // The encoder settled its output format: take it as it offers it.
                let t = unsafe { s.mft.GetOutputAvailableType(0, 0) }.map_err(err)?;
                unsafe { s.mft.SetOutputType(0, &t, 0) }.map_err(err)?;
                if let Some(h) = sequence_header(&t) { s.headers = h; }
                return Ok(None);
            }
            Err(e) => return Err(err(e)),
        }
        let sample = sample.ok_or("no output sample")?;
        let key = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) != 0;
        let mb = unsafe { sample.ConvertToContiguousBuffer() }.map_err(err)?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        unsafe { mb.Lock(&mut ptr, None, Some(&mut len)) }.map_err(err)?;
        let mut data = unsafe { std::slice::from_raw_parts(ptr, len as usize) }.to_vec();
        unsafe { let _ = mb.Unlock(); }
        if key {
            if crate::encode::has_nal(&data, 7) {
                // Keep the newest SPS and PPS for keyframes that come without them.
                s.headers = crate::encode::nal_units(&data).into_iter().filter(|(t, _)| *t == 7 || *t == 8).flat_map(|(_, u)| u.to_vec()).collect();
            } else if !s.headers.is_empty() {
                let mut d = s.headers.clone();
                d.extend_from_slice(&data);
                data = d;
            }
        }
        Ok(Some((data, key)))
    }
}

/// MF_MT_MPEG_SEQUENCE_HEADER (SPS + PPS, Annex-B) of a media type.
fn sequence_header(t: &IMFMediaType) -> Option<Vec<u8>> {
    let n = unsafe { t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }.ok()?;
    let mut b = vec![0u8; n as usize];
    unsafe { t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut b, None) }.ok()?;
    Some(b).filter(|b| !b.is_empty())
}

impl VideoEncoder for MfEncoder {
    fn name(&self) -> &'static str { self.name }

    fn set_bitrate(&mut self, bps: u32) {
        self.bitrate = bps;
        if let Some(api) = self.session.as_ref().and_then(|s| s.api.as_ref()) {
            let _ = unsafe { api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &u4(bps)) };
        }
    }

    /// Only the rate the encoder starts with; the frames' timestamps carry the real one.
    fn set_fps(&mut self, fps: f32) { self.fps = fps; }

    fn force_key(&mut self) { self.key = true; }

    /// `bgra` is unused: the picture is the backend's [GpuFrame].
    fn encode(&mut self, _bgra: &[u8], sw: usize, sh: usize, _stride: usize, w: usize, h: usize) -> Result<(Vec<u8>, bool), String> {
        let src = self.gpu.texture.borrow().as_ref().map(|(t, _, _)| t.clone()).ok_or("no frame on the GPU yet")?;
        let (sw, sh, w, h) = (sw as u32, sh as u32, w as u32, h as u32);
        if self.session.as_ref().is_none_or(|s| (s.w, s.h) != (w, h)) {
            self.session = None;
            self.session = Some(self.open(w, h)?);
            self.key = true;
        }
        if self.converter.as_ref().is_none_or(|c| c.size != (sw, sh, w, h)) {
            self.converter = Some(Converter::new(&self.gpu, sw, sh, w, h)?);
        }
        let nv12 = self.converter.as_mut().unwrap().convert(&src)?;
        let sample = unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &nv12, 0, false).map_err(err)?;
            if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
                if let Ok(n) = b2.GetContiguousLength() { let _ = buffer.SetCurrentLength(n); }
            }
            let sample = MFCreateSample().map_err(err)?;
            sample.AddBuffer(&buffer).map_err(err)?;
            sample.SetSampleTime(self.start.elapsed().as_micros() as i64 * 10).map_err(err)?;
            sample.SetSampleDuration((10_000_000.0 / self.fps.max(1.0)) as i64).map_err(err)?;
            sample
        };
        let key = std::mem::take(&mut self.key);
        let s = self.session.as_mut().unwrap();
        if key {
            if let Some(api) = &s.api { let _ = unsafe { api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &u4(1)) }; }
        }
        let deadline = Instant::now() + Duration::from_millis(500);
        while s.need_input == 0 {
            match Self::next_event(s, deadline)? {
                Some(t) if t == METransformNeedInput.0 as u32 => s.need_input += 1,
                // An output from a frame the last call gave up on: it is stale, drop it.
                Some(t) if t == METransformHaveOutput.0 as u32 => { let _ = Self::take_output(s)?; }
                Some(_) => {}
                None => { self.key |= key; return Err("the encoder asked for no input".into()); }
            }
        }
        unsafe { s.mft.ProcessInput(0, &sample, 0) }.map_err(err)?;
        s.need_input -= 1;
        let deadline = Instant::now() + Duration::from_millis(250);
        loop {
            match Self::next_event(s, deadline)? {
                Some(t) if t == METransformHaveOutput.0 as u32 => {
                    if let Some(out) = Self::take_output(s)? { return Ok(out); }
                }
                Some(t) if t == METransformNeedInput.0 as u32 => s.need_input += 1,
                Some(_) => {}
                // Nothing yet: this frame is skipped like one the rate control dropped.
                None => { self.key |= key; return Ok((Vec::new(), false)); }
            }
        }
    }
}


/// `encode-test [--frames N] [--out FILE]`: the hardware path without a desktop (it runs from an
/// SSH login too): a 4K picture with a moving bar drawn on the GPU, encoded at 2400 wide like a
/// phone asks for, timed. Writes the Annex-B stream to FILE for a decoder to check.
pub fn self_test(out: Option<&str>, frames: u32) -> i32 {
    let mut device = None;
    let mut context = None;
    let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
    if let Err(e) = unsafe {
        D3D11CreateDevice(None, windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE, Default::default(), flags, None,
            D3D11_SDK_VERSION, Some(&mut device), None, Some(&mut context))
    } { eprintln!("D3D11: {e}"); return 1; }
    let (device, context): (ID3D11Device, ID3D11DeviceContext) = (device.unwrap(), context.unwrap());
    let (sw, sh) = (3840u32, 2160u32);
    let (w, h) = crate::encode::fit(sw, sh, 2400);
    let gpu = Rc::new(GpuFrame { device: device.clone(), context: context.clone(), texture: Default::default() });
    let mut enc = match MfEncoder::new(gpu.clone()) {
        Ok(e) => e,
        Err(e) => { println!("{}", serde_json::json!({ "encoder": null, "error": e })); return 1; }
    };
    enc.set_bitrate(12_000_000);
    // The source texture, refilled from memory each frame: a grey ramp, text-like stripes and a bar.
    let src_desc = D3D11_TEXTURE2D_DESC {
        Width: sw, Height: sh, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT, BindFlags: 0, CPUAccessFlags: 0, MiscFlags: 0,
    };
    let mut src = None;
    if let Err(e) = unsafe { device.CreateTexture2D(&src_desc, None, Some(&mut src)) } { eprintln!("texture: {e}"); return 1; }
    let src = src.unwrap();
    let mut pixels = vec![0u8; (sw * sh * 4) as usize];
    let mut file = out.and_then(|p| std::fs::File::create(p).ok());
    let (mut times, mut bytes, mut keys, mut empty) = (Vec::new(), 0usize, 0u32, 0u32);
    for f in 0..frames {
        for y in 0..sh as usize {
            for x in 0..sw as usize {
                let i = (y * sw as usize + x) * 4;
                let bar = (x + f as usize * 16) % sw as usize > sw as usize - 200;
                let v = if bar { 255 } else if (y / 24) % 2 == 0 && (x / 9) % 3 != 0 { 40 } else { (x * 255 / sw as usize) as u8 };
                pixels[i] = v; pixels[i + 1] = if bar { 64 } else { v }; pixels[i + 2] = v; pixels[i + 3] = 255;
            }
        }
        unsafe { context.UpdateSubresource(&src, 0, None, pixels.as_ptr() as *const _, sw * 4, 0) };
        if let Err(e) = gpu.store(&src, sw, sh) { eprintln!("store: {e}"); return 1; }
        if f == 0 { enc.force_key(); }
        let t = Instant::now();
        match enc.encode(&[], sw as usize, sh as usize, 0, w, h) {
            Ok((data, key)) => {
                times.push(t.elapsed().as_micros() as u64);
                if data.is_empty() { empty += 1; continue; }
                bytes += data.len();
                keys += key as u32;
                if let Some(file) = &mut file { use std::io::Write; let _ = file.write_all(&data); }
            }
            Err(e) => { eprintln!("frame {f}: {e}"); return 1; }
        }
    }
    times.sort_unstable();
    let avg = times.iter().sum::<u64>() / times.len().max(1) as u64;
    println!("{}", serde_json::json!({
        "encoder": enc.name(), "size": format!("{w}x{h}"), "frames": frames, "empty": empty, "keys": keys, "bytes": bytes,
        "encode_us": { "avg": avg, "p50": times.get(times.len() / 2), "p95": times.get(times.len() * 95 / 100), "max": times.last() },
    }));
    0
}
