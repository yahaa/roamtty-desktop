//! Software H.264 through Cisco's OpenH264 binary, downloaded at install time so Cisco's patent
//! licence covers it. Scaling and BGRA→I420 happen in one pass.

use openh264::encoder::{BitRate, Complexity, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode, SpsPpsStrategy, UsageType};
use openh264::formats::YUVBuffer;
use openh264::OpenH264API;
use openh264_sys2::{SBitrateInfo, ENCODER_OPTION_BITRATE, ENCODER_OPTION_FRAME_RATE, ENCODER_OPTION_MAX_BITRATE, SPATIAL_LAYER_ALL};
use std::path::{Path, PathBuf};

/// The Cisco file name for this machine, e.g. `libopenh264-2.6.0-linux64.8.so`.
pub fn openh264_name() -> Option<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Some("libopenh264-2.6.0-linux64.8.so"),
        "aarch64" => Some("libopenh264-2.6.0-linux-arm64.8.so"),
        _ => None,
    }
}

pub fn openh264_path() -> Option<PathBuf> {
    Some(crate::paths::lib_dir().join(openh264_name()?))
}

pub struct H264 {
    enc: Option<Encoder>,
    api: PathBuf,
    w: usize,
    h: usize,
    bitrate: u32,
    fps: f32,
}

impl H264 {
    pub fn new(lib: &Path) -> Result<H264, String> {
        // Checked against the hashes of Cisco's releases before it is ever loaded.
        OpenH264API::from_blob_path(lib).map_err(|e| format!("openh264: {e}"))?;
        Ok(H264 { enc: None, api: lib.to_path_buf(), w: 0, h: 0, bitrate: 4_000_000, fps: 30.0 })
    }

    fn make(&self) -> Result<Encoder, String> {
        let api = OpenH264API::from_blob_path(&self.api).map_err(|e| e.to_string())?;
        let cfg = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(self.bitrate))
            .max_frame_rate(FrameRate::from_hz(self.fps))
            .skip_frames(true)
            .complexity(Complexity::Low)
            .scene_change_detect(true)
            .background_detection(false)
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            .intra_frame_period(IntraFramePeriod::from_num_frames(0))
            .num_threads(2);
        Encoder::with_api_config(api, cfg).map_err(|e| e.to_string())
    }

    pub fn set_bitrate(&mut self, bps: u32) {
        self.bitrate = bps;
        if let Some(e) = &mut self.enc {
            let mut info = SBitrateInfo { iLayer: SPATIAL_LAYER_ALL, iBitrate: bps as i32 };
            unsafe {
                let api = e.raw_api();
                api.set_option(ENCODER_OPTION_MAX_BITRATE, (&raw mut info).cast());
                api.set_option(ENCODER_OPTION_BITRATE, (&raw mut info).cast());
            }
        }
    }

    pub fn set_fps(&mut self, fps: f32) {
        self.fps = fps;
        if let Some(e) = &mut self.enc {
            let mut f = fps;
            unsafe { e.raw_api().set_option(ENCODER_OPTION_FRAME_RATE, (&raw mut f).cast()); }
        }
    }

    pub fn force_key(&mut self) {
        if let Some(e) = &mut self.enc { e.force_intra_frame(); }
    }

    /// Encodes `bgra` scaled to `w`×`h`; returns Annex-B and whether it is a keyframe.
    pub fn encode(&mut self, bgra: &[u8], sw: usize, sh: usize, stride: usize, w: usize, h: usize) -> Result<(Vec<u8>, bool), String> {
        if self.enc.is_none() || (w, h) != (self.w, self.h) {
            self.enc = Some(self.make()?);
            self.w = w;
            self.h = h;
        }
        let mut yuv = Vec::new();
        to_i420(bgra, sw, sh, stride, w, h, &mut yuv);
        let src = YUVBuffer::from_vec(yuv, w, h);
        let enc = self.enc.as_mut().unwrap();
        let out = enc.encode(&src).map_err(|e| e.to_string())?;
        let key = matches!(out.frame_type(), openh264::encoder::FrameType::IDR | openh264::encoder::FrameType::I);
        let data = out.to_vec();
        Ok((data, key))
    }
}

/// Box-filtered downscale (or nearest upscale) from BGRA into planar I420, BT.601 limited range,
/// which is what decoders assume when the stream carries no colour description.
pub fn to_i420(bgra: &[u8], sw: usize, sh: usize, stride: usize, w: usize, h: usize, out: &mut Vec<u8>) {
    out.resize(w * h * 3 / 2, 0);
    let (y_plane, uv) = out.split_at_mut(w * h);
    let (u_plane, v_plane) = uv.split_at_mut(w * h / 4);
    // Fixed-point source span per output pixel.
    let fx = ((sw as u64) << 16) / w as u64;
    let fy = ((sh as u64) << 16) / h as u64;
    let mut rgb_row = vec![[0i32; 3]; w * 2];
    for oy in (0..h).step_by(2) {
        for r in 0..2 {
            let y = oy + r;
            let y0 = ((y as u64 * fy) >> 16) as usize;
            let y1 = ((((y + 1) as u64 * fy) >> 16) as usize).clamp(y0 + 1, sh);
            for x in 0..w {
                let x0 = ((x as u64 * fx) >> 16) as usize;
                let x1 = ((((x + 1) as u64 * fx) >> 16) as usize).clamp(x0 + 1, sw);
                // At most 2×2 samples per pixel keeps this cheap; enough for text at the ratios we use.
                let ys = [y0, (y1 - 1).max(y0)];
                let xs = [x0, (x1 - 1).max(x0)];
                let (mut b, mut g, mut rr) = (0i32, 0i32, 0i32);
                for yy in ys {
                    let row = &bgra[yy * stride..];
                    for xx in xs {
                        let p = &row[xx * 4..xx * 4 + 3];
                        b += p[0] as i32;
                        g += p[1] as i32;
                        rr += p[2] as i32;
                    }
                }
                let (rr, g, b) = (rr >> 2, g >> 2, b >> 2);
                rgb_row[r * w + x] = [rr, g, b];
                y_plane[y * w + x] = (((66 * rr + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
            }
        }
        for x in (0..w).step_by(2) {
            let mut s = [0i32; 3];
            for (r, c) in [(0, x), (0, x + 1), (1, x), (1, x + 1)] {
                let p = rgb_row[r * w + c];
                s[0] += p[0];
                s[1] += p[1];
                s[2] += p[2];
            }
            let (rr, g, b) = (s[0] >> 2, s[1] >> 2, s[2] >> 2);
            let i = (oy / 2) * (w / 2) + x / 2;
            u_plane[i] = (((-38 * rr - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
            v_plane[i] = (((112 * rr - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
        }
    }
}

/// Picks the encode size: the native size scaled so the long edge is at most `long`, even sides.
pub fn fit(native_w: u32, native_h: u32, long: u32) -> (usize, usize) {
    let edge = native_w.max(native_h);
    let long = long.min(edge).max(16);
    let w = (native_w as u64 * long as u64 / edge as u64) as usize;
    let h = (native_h as u64 * long as u64 / edge as u64) as usize;
    (w.max(16) & !1, h.max(16) & !1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_ratio_and_even_sides() {
        assert_eq!(fit(3024, 1964, 1920), (1920, 1246));
        assert_eq!(fit(1280, 720, 2400), (1280, 720));
        assert_eq!(fit(1921, 1081, 1921), (1920, 1080));
    }

    #[test]
    fn i420_of_white_and_black() {
        let white = vec![255u8; 4 * 4 * 4];
        let mut out = Vec::new();
        to_i420(&white, 4, 4, 16, 2, 2, &mut out);
        assert_eq!(out.len(), 6);
        assert_eq!(out[0], 235);
        assert_eq!(out[4], 128);
        let black = vec![0u8; 4 * 4 * 4];
        to_i420(&black, 4, 4, 16, 4, 4, &mut out);
        assert_eq!(out[0], 16);
    }
}
