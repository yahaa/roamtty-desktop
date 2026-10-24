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
    /// The part of the last [encode](VideoEncoder::encode) spent scaling and converting pixels, in µs.
    fn convert_us(&self) -> u64 { 0 }
}

/// Box-filtered downscale (or nearest upscale) from BGRA into planar I420, BT.601 limited range,
/// which is what decoders assume when the stream carries no colour description. Bands of rows go
/// to up to four threads of a pool that lives as long as the process: a 4K screen took 14 ms on one
/// core, more than the encoder itself, and spawning the threads anew cost every frame ~0.1 ms.
pub fn to_i420(bgra: &[u8], sw: usize, sh: usize, stride: usize, w: usize, h: usize, out: &mut Vec<u8>) {
    out.resize(w * h * 3 / 2, 0);
    let (y_plane, uv) = out.split_at_mut(w * h);
    let (u_plane, v_plane) = uv.split_at_mut(w * h / 4);
    let threads = pool::size();
    // Whole row pairs per band, so each band owns its rows of every plane.
    let band = (h / 2).div_ceil(threads).max(1) * 2;
    // The (at most two) source columns of each output column, as byte offsets.
    let fx = ((sw as u64) << 16) / w as u64;
    let cols: Vec<(usize, usize)> = (0..w).map(|x| {
        let x0 = ((x as u64 * fx) >> 16) as usize;
        let x1 = ((((x + 1) as u64 * fx) >> 16) as usize).clamp(x0 + 1, sw);
        (x0 * 4, (x1 - 1).max(x0) * 4)
    }).collect();
    let bands: Vec<_> = y_plane.chunks_mut(band * w).zip(u_plane.chunks_mut(band * w / 4)).zip(v_plane.chunks_mut(band * w / 4))
        .map(|((y, u), v)| std::sync::Mutex::new((y, u, v))).collect();
    pool::run(bands.len(), &|i| {
        let mut b = bands[i].lock().unwrap();
        let (y, u, v) = &mut *b;
        to_i420_rows(bgra, sw, sh, stride, w, h, &cols, i * band, y, u, v);
    });
}

/// The output rows from `top` on that fit in `y_plane` (with their halves of U and V).
#[allow(clippy::too_many_arguments)]
fn to_i420_rows(bgra: &[u8], sw: usize, sh: usize, stride: usize, w: usize, h: usize, cols: &[(usize, usize)], top: usize,
    y_plane: &mut [u8], u_plane: &mut [u8], v_plane: &mut [u8]) {
    let fy = ((sh as u64) << 16) / h as u64;
    let same = sw == w && sh == h;
    // Each output pixel's R, G, B for the row pair, for the chroma average.
    let mut rgb_row = vec![[0i32; 3]; w * 2];
    let rows = y_plane.len() / w;
    for oy in (0..rows).step_by(2) {
        for r in 0..2 {
            let y = top + oy + r;
            let y_out = &mut y_plane[(oy + r) * w..(oy + r + 1) * w];
            let rgb = &mut rgb_row[r * w..(r + 1) * w];
            if same {
                // Native size: one source pixel per output pixel, and no averaging to do.
                let src = &bgra[y * stride..y * stride + w * 4];
                for ((p, yo), c) in src.chunks_exact(4).zip(y_out.iter_mut()).zip(rgb.iter_mut()) {
                    let (b, g, rr) = (p[0] as i32, p[1] as i32, p[2] as i32);
                    *c = [rr, g, b];
                    *yo = (((66 * rr + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
                }
                continue;
            }
            let y0 = ((y as u64 * fy) >> 16) as usize;
            let y1 = ((((y + 1) as u64 * fy) >> 16) as usize).clamp(y0 + 1, sh);
            // At most 2×2 samples per pixel keeps this cheap; enough for text at the ratios we use.
            let ra = &bgra[y0 * stride..];
            let rb = &bgra[(y1 - 1).max(y0) * stride..];
            for ((&(xa, xb), yo), c) in cols.iter().zip(y_out.iter_mut()).zip(rgb.iter_mut()) {
                let b = ra[xa] as i32 + ra[xb] as i32 + rb[xa] as i32 + rb[xb] as i32;
                let g = ra[xa + 1] as i32 + ra[xb + 1] as i32 + rb[xa + 1] as i32 + rb[xb + 1] as i32;
                let rr = ra[xa + 2] as i32 + ra[xb + 2] as i32 + rb[xa + 2] as i32 + rb[xb + 2] as i32;
                let (rr, g, b) = (rr >> 2, g >> 2, b >> 2);
                *c = [rr, g, b];
                *yo = (((66 * rr + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
            }
        }
        let (top_row, bottom_row) = rgb_row.split_at(w);
        let base = (oy / 2) * (w / 2);
        let u_out = &mut u_plane[base..base + w / 2];
        let v_out = &mut v_plane[base..base + w / 2];
        for (((t, bt), uo), vo) in top_row.chunks_exact(2).zip(bottom_row.chunks_exact(2)).zip(u_out.iter_mut()).zip(v_out.iter_mut()) {
            let rr = (t[0][0] + t[1][0] + bt[0][0] + bt[1][0]) >> 2;
            let g = (t[0][1] + t[1][1] + bt[0][1] + bt[1][1]) >> 2;
            let b = (t[0][2] + t[1][2] + bt[0][2] + bt[1][2]) >> 2;
            *uo = (((-38 * rr - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
            *vo = (((112 * rr - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
        }
    }
}

/// Worker threads that live as long as the process, for work split into a few parts per frame.
mod pool {
    use std::sync::mpsc::{channel, Sender};
    use std::sync::{Mutex, OnceLock};

    /// One part of a [run]: the caller waits for every part before `f` goes out of scope.
    struct Job { f: *const (dyn Fn(usize) + Sync), part: usize, done: Sender<()> }
    // SAFETY: `f` is Sync, and `run` keeps it alive until the job reported back.
    unsafe impl Send for Job {}

    fn workers() -> &'static [Mutex<Sender<Job>>] {
        static W: OnceLock<Vec<Mutex<Sender<Job>>>> = OnceLock::new();
        W.get_or_init(|| (1..size()).map(|_| {
            let (tx, rx) = channel::<Job>();
            std::thread::spawn(move || {
                for j in rx {
                    let f = unsafe { &*j.f };
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(j.part)));
                    let _ = j.done.send(());
                }
            });
            Mutex::new(tx)
        }).collect())
    }

    /// How many parts [run] does at once: the caller's thread and up to three workers.
    pub fn size() -> usize { std::thread::available_parallelism().map_or(1, |n| n.get()).min(4) }

    /// Calls `f(0..parts)`, part 0 on this thread and the rest on the workers, and returns once all are done.
    pub fn run(parts: usize, f: &(dyn Fn(usize) + Sync)) {
        let workers = workers();
        let (tx, rx) = channel();
        let mut sent = 0;
        for part in 1..parts {
            // SAFETY: lifetime erased only until the `recv`s below, which wait for every job sent.
            let job = Job { f: unsafe { std::mem::transmute::<&(dyn Fn(usize) + Sync), &'static (dyn Fn(usize) + Sync)>(f) }, part, done: tx.clone() };
            match workers.get(part - 1) {
                Some(w) if w.lock().unwrap().send(job).is_ok() => sent += 1,
                _ => f(part),
            }
        }
        if parts > 0 { f(0); }
        for _ in 0..sent { let _ = rx.recv(); }
    }
}

/// The NAL units of an Annex-B access unit: (type, the unit with its start code).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn nal_units(data: &[u8]) -> Vec<(u8, &[u8])> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push((if i > 0 && data[i - 1] == 0 { i - 1 } else { i }, i + 3));
            i += 3;
        } else { i += 1; }
    }
    starts.iter().enumerate().map(|(n, &(start, body))| {
        let end = starts.get(n + 1).map_or(data.len(), |s| s.0);
        (data.get(body).map_or(0, |b| b & 0x1F), &data[start..end])
    }).collect()
}

#[cfg_attr(not(windows), allow(dead_code))]
pub fn has_nal(data: &[u8], ty: u8) -> bool { nal_units(data).iter().any(|(t, _)| *t == ty) }

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

    #[test]
    fn nal_units_of_an_access_unit() {
        let au = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9];
        let units = nal_units(&au);
        assert_eq!(units.iter().map(|u| u.0).collect::<Vec<_>>(), vec![7, 8, 5]);
        assert_eq!(units[0].1, &[0, 0, 0, 1, 0x67, 1, 2]);
        assert_eq!(units[1].1, &[0, 0, 1, 0x68, 3]);
        assert!(has_nal(&au, 5) && !has_nal(&au, 1));
    }

    /// The banded, table-driven conversion (and its native-size path) gives exactly the picture the
    /// plain per-pixel one gives, for downscales, upscales, native size and padded rows.
    #[test]
    fn i420_matches_the_reference() {
        for (sw, sh, stride, w, h) in [(333usize, 211usize, 333 * 4, 202usize, 130usize), (64, 48, 64 * 4 + 32, 64, 48),
            (100, 60, 400, 160, 96), (3840, 2160, 3840 * 4, 2400, 1350), (17 * 2, 9 * 2, 136, 34, 18)] {
            let bgra: Vec<u8> = (0..stride * sh).map(|i| (i * 7919 % 251) as u8).collect();
            let mut banded = Vec::new();
            to_i420(&bgra, sw, sh, stride, w, h, &mut banded);
            let mut whole = vec![0u8; w * h * 3 / 2];
            let (y, uv) = whole.split_at_mut(w * h);
            let (u, v) = uv.split_at_mut(w * h / 4);
            to_i420_reference(&bgra, sw, sh, stride, w, h, y, u, v);
            assert!(banded == whole, "{sw}x{sh} -> {w}x{h}");
        }
    }

    /// The conversion as it was before the column tables and the native-size path.
    #[allow(clippy::too_many_arguments)]
    fn to_i420_reference(bgra: &[u8], sw: usize, sh: usize, stride: usize, w: usize, h: usize,
        y_plane: &mut [u8], u_plane: &mut [u8], v_plane: &mut [u8]) {
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
                    let (mut b, mut g, mut rr) = (0i32, 0i32, 0i32);
                    for yy in [y0, (y1 - 1).max(y0)] {
                        for xx in [x0, (x1 - 1).max(x0)] {
                            let p = &bgra[yy * stride + xx * 4..];
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
                    for k in 0..3 { s[k] += rgb_row[r * w + c][k]; }
                }
                let (rr, g, b) = (s[0] >> 2, s[1] >> 2, s[2] >> 2);
                let i = (oy / 2) * (w / 2) + x / 2;
                u_plane[i] = (((-38 * rr - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
                v_plane[i] = (((112 * rr - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
            }
        }
    }
}

#[cfg(test)]
mod bench {
    /// `cargo test --release -- --ignored --nocapture i420_speed`
    #[test]
    #[ignore]
    fn i420_speed() {
        for (sw, sh, w, h) in [(1920usize, 1080usize, 1920usize, 1080usize), (3840, 2160, 2400, 1350), (2880, 1800, 1920, 1200), (1920, 1080, 1280, 720)] {
            let bgra: Vec<u8> = (0..sw * sh * 4).map(|i| (i * 7919 % 251) as u8).collect();
            let mut out = Vec::new();
            super::to_i420(&bgra, sw, sh, sw * 4, w, h, &mut out);
            let n = 50;
            let t = std::time::Instant::now();
            for _ in 0..n { super::to_i420(&bgra, sw, sh, sw * 4, w, h, &mut out); }
            println!("{sw}x{sh} -> {w}x{h}: {:.2} ms", t.elapsed().as_secs_f64() * 1000.0 / n as f64);
        }
    }
}
