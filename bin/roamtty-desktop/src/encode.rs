//! Encoding helpers shared by every platform: the encode size and BGRA → I420 scaling, plus the
//! encoder interface (OpenH264 on Linux, VideoToolbox on macOS, Media Foundation on Windows).

/// One H.264 encoder: low latency, no B-frames, keyframes only when asked.
pub trait VideoEncoder {
    fn name(&self) -> &'static str;
    fn set_bitrate(&mut self, bps: u32);
    fn set_fps(&mut self, fps: f32);
    fn force_key(&mut self);
    /// Encodes `bgra` scaled to `w`×`h`; returns Annex-B and whether it is a keyframe.
    fn encode(&mut self, bgra: &[u8], sw: usize, sh: usize, stride: usize, w: usize, h: usize) -> Result<(Vec<u8>, bool), String>;
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
