//! Software H.264 through Cisco's OpenH264 binary, downloaded at install time so Cisco's patent
//! licence covers it. Linux only: elsewhere the system encoder is used.

use crate::encode::{to_i420, VideoEncoder};
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

}

impl VideoEncoder for H264 {
    fn name(&self) -> &'static str { "openh264" }

    fn set_bitrate(&mut self, bps: u32) {
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

    fn set_fps(&mut self, fps: f32) {
        self.fps = fps;
        if let Some(e) = &mut self.enc {
            let mut f = fps;
            unsafe { e.raw_api().set_option(ENCODER_OPTION_FRAME_RATE, (&raw mut f).cast()); }
        }
    }

    fn force_key(&mut self) {
        if let Some(e) = &mut self.enc { e.force_intra_frame(); }
    }

    fn encode(&mut self, bgra: &[u8], sw: usize, sh: usize, stride: usize, w: usize, h: usize) -> Result<(Vec<u8>, bool), String> {
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
