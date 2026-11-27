//! Real-time Opus VOIP codec, adaptive RED + bitrate controller, speech jitter buffer,
//! and WSOLA time-scale convergence for `roamtty-desktop voice`.

use proto::{voice_flags, VoiceOpusPacket, VoiceRedEntry, VoiceStats, VOICE_MAX_RED_DELTA, VOICE_MAX_RED_ENTRIES};
use std::collections::{BTreeMap, VecDeque};

pub const FRAME_MS: u32 = 20;
const SILENCE_RMS_THRESHOLD: f32 = 0.004;
const MAX_CONSECUTIVE_PLC: u32 = 5;
const STATS_WINDOW_MS: u64 = 1_000;
const JITTER_WINDOW: usize = 150;
const STEP_DOWN_HOLD_MS: u64 = 3_000;

/// Sample rates natively supported by Opus (8/12/16/24/48 kHz).
pub fn is_opus_sample_rate(rate: u32) -> bool {
    matches!(rate, 8_000 | 12_000 | 16_000 | 24_000 | 48_000)
}

pub fn frame_samples(rate: u32) -> usize {
    (rate as usize * FRAME_MS as usize) / 1000
}

pub fn pcm_rms(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sum = 0.0f64;
    for &s in samples {
        let f = s as f64 / 32768.0;
        sum += f * f;
    }
    (sum / samples.len() as f64).sqrt() as f32
}

pub fn s16le_bytes_to_i16(bytes: &[u8], out: &mut Vec<i16>) {
    out.clear();
    out.reserve(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        out.push(i16::from_le_bytes([pair[0], pair[1]]));
    }
}

pub fn i16_to_s16le_bytes(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

// ============================================================================
// libopus FFI wrappers (VOIP mode with In-band FEC, DTX, PLC, dynamic bitrate)
// ============================================================================

pub struct VoiceOpusEncoder {
    ptr: *mut audiopus_sys::OpusEncoder,
    frame_len: usize,
    bitrate: i32,
    loss_perc: i32,
}

unsafe impl Send for VoiceOpusEncoder {}

impl VoiceOpusEncoder {
    pub fn new(sample_rate: u32, bitrate: i32, dtx: bool) -> Result<Self, String> {
        use audiopus_sys::*;
        if !is_opus_sample_rate(sample_rate) {
            return Err(format!("unsupported opus rate {sample_rate}"));
        }
        let mut err = 0;
        let ptr = unsafe { opus_encoder_create(sample_rate as i32, 1, OPUS_APPLICATION_VOIP, &mut err) };
        if ptr.is_null() || err != OPUS_OK {
            return Err(format!("opus_encoder_create({sample_rate}) failed: {err}"));
        }
        unsafe {
            opus_encoder_ctl(ptr, OPUS_SET_BITRATE_REQUEST, bitrate);
            opus_encoder_ctl(ptr, OPUS_SET_SIGNAL_REQUEST, OPUS_SIGNAL_VOICE);
            opus_encoder_ctl(ptr, OPUS_SET_COMPLEXITY_REQUEST, 5i32);
            opus_encoder_ctl(ptr, OPUS_SET_INBAND_FEC_REQUEST, 1i32);
            opus_encoder_ctl(ptr, OPUS_SET_PACKET_LOSS_PERC_REQUEST, 5i32);
            opus_encoder_ctl(ptr, OPUS_SET_DTX_REQUEST, if dtx { 1i32 } else { 0i32 });
        }
        Ok(Self {
            ptr,
            frame_len: frame_samples(sample_rate),
            bitrate,
            loss_perc: 5,
        })
    }

    pub fn frame_len(&self) -> usize {
        self.frame_len
    }

    pub fn apply_params(&mut self, bitrate: i32, loss_perc: i32) {
        use audiopus_sys::*;
        if bitrate != self.bitrate {
            self.bitrate = bitrate;
            unsafe {
                opus_encoder_ctl(self.ptr, OPUS_SET_BITRATE_REQUEST, bitrate);
            }
        }
        let clamped_loss = loss_perc.clamp(0, 100);
        if clamped_loss != self.loss_perc {
            self.loss_perc = clamped_loss;
            unsafe {
                opus_encoder_ctl(self.ptr, OPUS_SET_PACKET_LOSS_PERC_REQUEST, clamped_loss);
            }
        }
    }

    pub fn reset(&mut self) {
        unsafe {
            audiopus_sys::opus_encoder_ctl(self.ptr, audiopus_sys::OPUS_RESET_STATE);
        }
    }

    pub fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>, String> {
        if pcm.len() != self.frame_len {
            return Err(format!("expected {} samples, got {}", self.frame_len, pcm.len()));
        }
        let mut out = vec![0u8; 512];
        let n = unsafe {
            audiopus_sys::opus_encode(
                self.ptr,
                pcm.as_ptr(),
                self.frame_len as i32,
                out.as_mut_ptr(),
                out.len() as i32,
            )
        };
        if n < 0 {
            return Err(format!("opus_encode failed: {n}"));
        }
        out.truncate(n as usize);
        Ok(out)
    }
}

impl Drop for VoiceOpusEncoder {
    fn drop(&mut self) {
        unsafe {
            audiopus_sys::opus_encoder_destroy(self.ptr);
        }
    }
}

pub struct VoiceOpusDecoder {
    ptr: *mut audiopus_sys::OpusDecoder,
    frame_len: usize,
}

unsafe impl Send for VoiceOpusDecoder {}

impl VoiceOpusDecoder {
    pub fn new(sample_rate: u32) -> Result<Self, String> {
        use audiopus_sys::*;
        if !is_opus_sample_rate(sample_rate) {
            return Err(format!("unsupported opus decode rate {sample_rate}"));
        }
        let mut err = 0;
        let ptr = unsafe { opus_decoder_create(sample_rate as i32, 1, &mut err) };
        if ptr.is_null() || err != OPUS_OK {
            return Err(format!("opus_decoder_create({sample_rate}) failed: {err}"));
        }
        Ok(Self {
            ptr,
            frame_len: frame_samples(sample_rate),
        })
    }

    pub fn frame_len(&self) -> usize {
        self.frame_len
    }

    pub fn decode(&mut self, packet: &[u8], out: &mut Vec<i16>, decode_fec: bool) -> Result<usize, String> {
        out.resize(self.frame_len, 0);
        let n = unsafe {
            audiopus_sys::opus_decode(
                self.ptr,
                packet.as_ptr(),
                packet.len() as i32,
                out.as_mut_ptr(),
                self.frame_len as i32,
                if decode_fec { 1 } else { 0 },
            )
        };
        if n < 0 {
            return Err(format!("opus_decode failed: {n}"));
        }
        out.truncate(n as usize);
        Ok(n as usize)
    }

    pub fn decode_plc(&mut self, out: &mut Vec<i16>) -> Result<usize, String> {
        out.resize(self.frame_len, 0);
        let n = unsafe {
            audiopus_sys::opus_decode(
                self.ptr,
                std::ptr::null(),
                0,
                out.as_mut_ptr(),
                self.frame_len as i32,
                0,
            )
        };
        if n < 0 {
            return Err(format!("opus_decode PLC failed: {n}"));
        }
        out.truncate(n as usize);
        Ok(n as usize)
    }

    pub fn reset(&mut self) {
        unsafe {
            audiopus_sys::opus_decoder_ctl(self.ptr, audiopus_sys::OPUS_RESET_STATE);
        }
    }
}

impl Drop for VoiceOpusDecoder {
    fn drop(&mut self) {
        unsafe {
            audiopus_sys::opus_decoder_destroy(self.ptr);
        }
    }
}

// ============================================================================
// Adaptive Bitrate + RED Depth + Opus LBRR Controller
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkProfile {
    pub bitrate: i32,
    pub red_count: usize,
    pub opus_loss_perc: i32,
}

/// Adapts Opus bitrate, in-packet RED depth (0..=3), and Opus in-band FEC percentage from peer `VoiceStats`.
/// Escalates protection immediately on loss/burst; steps down only after 3 s of clean network reports.
pub struct VoiceLinkController {
    base_bitrate: i32,
    tier: u8, // 0=excellent, 1=light, 2=moderate, 3=extreme
    smoothed_loss: f64,
    smoothed_burst: f64,
    clean_since_ms: Option<u64>,
}

impl VoiceLinkController {
    pub fn for_sample_rate(sample_rate: u32) -> Self {
        let base_bitrate = match sample_rate {
            r if r <= 12_000 => 18_000,
            16_000 => 28_000,
            24_000 => 36_000,
            _ => 48_000,
        };
        Self {
            base_bitrate,
            tier: 0,
            smoothed_loss: 0.0,
            smoothed_burst: 0.0,
            clean_since_ms: None,
        }
    }

    pub fn profile(&self) -> LinkProfile {
        let (scale, red_count, opus_loss_perc) = match self.tier {
            0 => (1.00, 0, 5),
            1 => (0.86, 1, 10),
            2 => (0.66, 2, 20),
            _ => (0.45, 3, 30),
        };
        let bitrate = ((self.base_bitrate as f64 * scale) as i32).clamp(12_000, 64_000);
        LinkProfile {
            bitrate,
            red_count,
            opus_loss_perc,
        }
    }

    pub fn on_stats(&mut self, st: &VoiceStats, now_ms: u64) -> LinkProfile {
        let loss = (st.loss_permille as f64) / 1000.0;
        let burst = st.max_burst_loss as f64;
        // Fast attack, slow decay so brief spikes immediately arm RED and hold briefly.
        self.smoothed_loss = if loss > self.smoothed_loss {
            self.smoothed_loss * 0.4 + loss * 0.6
        } else {
            self.smoothed_loss * 0.85 + loss * 0.15
        };
        self.smoothed_burst = if burst > self.smoothed_burst {
            burst
        } else {
            self.smoothed_burst * 0.80 + burst * 0.20
        };
        let desired = if self.smoothed_loss >= 0.15 || self.smoothed_burst >= 3.0 {
            3u8
        } else if self.smoothed_loss >= 0.05 || self.smoothed_burst >= 2.0 {
            2u8
        } else if self.smoothed_loss >= 0.01 || self.smoothed_burst >= 1.0 || st.jitter_ms >= 35 {
            1u8
        } else {
            0u8
        };

        if desired > self.tier {
            self.tier = desired;
            self.clean_since_ms = None;
        } else if desired < self.tier {
            let since = *self.clean_since_ms.get_or_insert(now_ms);
            if now_ms.saturating_sub(since) >= STEP_DOWN_HOLD_MS {
                self.tier = self.tier.saturating_sub(1);
                self.clean_since_ms = Some(now_ms);
            }
        } else {
            self.clean_since_ms = None;
        }
        self.profile()
    }
}

/// Rolling buffer of recent primary Opus packets for constructing RFC 2198-style RED entries.
#[derive(Default)]
pub struct RedHistory {
    recent: VecDeque<(u32, Vec<u8>)>,
}

impl RedHistory {
    pub fn clear(&mut self) {
        self.recent.clear();
    }

    pub fn build_red(&self, seq: u32, max_count: usize) -> Vec<VoiceRedEntry> {
        if max_count == 0 || self.recent.is_empty() {
            return Vec::new();
        }
        let want = max_count.min(VOICE_MAX_RED_ENTRIES);
        let mut out = Vec::with_capacity(want);
        for (prev_seq, data) in self.recent.iter().rev() {
            let delta = seq.wrapping_sub(*prev_seq);
            if delta == 0 || delta > VOICE_MAX_RED_DELTA as u32 {
                continue;
            }
            out.push(VoiceRedEntry {
                delta_seq: delta as u8,
                data: data.clone(),
            });
            if out.len() >= want {
                break;
            }
        }
        out
    }

    pub fn record(&mut self, seq: u32, primary: Vec<u8>) {
        if primary.is_empty() {
            return;
        }
        self.recent.push_back((seq, primary));
        while self.recent.len() > 4 {
            self.recent.pop_front();
        }
    }
}

// ============================================================================
// Receiver Loss / Burst / Jitter Estimator (measures RAW primary packets!)
// ============================================================================

#[derive(Default)]
pub struct VoiceRxStats {
    max_seq: Option<u32>,
    window_events: VecDeque<(u64, bool)>, // (at_ms, received_primary)
    burst_events: VecDeque<(u64, u8)>,
    current_loss_streak: u8,
    seen_seqs: VecDeque<u32>,
    arrival_at: VecDeque<i64>,
    arrival_offset: VecDeque<i64>,
    ewma_jitter_ms: f64,
    last_transit_ms: Option<i64>,
}

impl VoiceRxStats {
    /// Resets sequence continuity after a barge-in (`SPK_CLEAR`) so skipped turn sequences are not counted as loss.
    pub fn reset_seq_cursor(&mut self) {
        self.max_seq = None;
        self.current_loss_streak = 0;
        self.reset_timing();
    }

    /// Resets interarrival jitter baseline when a stream goes idle between utterances so pauses aren't measured as jitter.
    pub fn reset_timing(&mut self) {
        self.last_transit_ms = None;
        self.arrival_at.clear();
        self.arrival_offset.clear();
    }

    pub fn on_primary(&mut self, seq: u32, now_ms: u64, reset_discontinuity: bool) {
        if self.seen_seqs.contains(&seq) {
            return;
        }
        self.seen_seqs.push_back(seq);
        while self.seen_seqs.len() > 256 {
            self.seen_seqs.pop_front();
        }

        // Update RFC-3550 style interarrival jitter and 3s P95 residual window.
        let transit = now_ms as i64 - (seq as i64) * (FRAME_MS as i64);
        if reset_discontinuity {
            self.last_transit_ms = Some(transit);
        } else {
            if let Some(prev) = self.last_transit_ms {
                let d = (transit - prev).abs() as f64;
                self.ewma_jitter_ms += (d - self.ewma_jitter_ms) / 16.0;
            }
            self.last_transit_ms = Some(transit);
            if let Some(&last_o) = self.arrival_offset.back() {
                if (transit - last_o).abs() > 2_000 {
                    self.arrival_at.clear();
                    self.arrival_offset.clear();
                }
            }
            self.arrival_at.push_back(now_ms as i64);
            self.arrival_offset.push_back(transit);
            while self.arrival_at.len() > JITTER_WINDOW {
                self.arrival_at.pop_front();
                self.arrival_offset.pop_front();
            }
        }

        match self.max_seq {
            None => {
                self.max_seq = Some(seq);
                self.window_events.push_back((now_ms, true));
                self.current_loss_streak = 0;
            }
            Some(_) if reset_discontinuity => {
                self.max_seq = Some(seq);
                self.window_events.push_back((now_ms, true));
                self.current_loss_streak = 0;
            }
            Some(prev_max) => {
                let diff = seq.wrapping_sub(prev_max) as i32;
                if diff > 0 && diff <= 200 {
                    let missing = (diff - 1) as usize;
                    for _ in 0..missing {
                        self.window_events.push_back((now_ms, false));
                        self.current_loss_streak = self.current_loss_streak.saturating_add(1);
                    }
                    if self.current_loss_streak > 0 {
                        self.burst_events.push_back((now_ms, self.current_loss_streak));
                        self.current_loss_streak = 0;
                    }
                    self.window_events.push_back((now_ms, true));
                    self.max_seq = Some(seq);
                } else if diff > 200 {
                    // Stream reset / discontinuity.
                    self.max_seq = Some(seq);
                    self.window_events.clear();
                    self.burst_events.clear();
                    self.window_events.push_back((now_ms, true));
                    self.current_loss_streak = 0;
                } else if (-64..0).contains(&diff) {
                    // Late out-of-order primary packet arrived: flip one earlier missing entry back to received.
                    if let Some(slot) = self.window_events.iter_mut().rev().find(|(_, rx)| !*rx) {
                        slot.1 = true;
                    }
                }
            }
        }
        self.prune(now_ms);
    }

    fn prune(&mut self, now_ms: u64) {
        while self.window_events.front().is_some_and(|(t, _)| now_ms.saturating_sub(*t) > STATS_WINDOW_MS) {
            self.window_events.pop_front();
        }
        while self.burst_events.front().is_some_and(|(t, _)| now_ms.saturating_sub(*t) > STATS_WINDOW_MS * 2) {
            self.burst_events.pop_front();
        }
    }

    pub fn p95_jitter_ms(&self) -> f64 {
        let count = self.arrival_at.len();
        if count < 4 {
            return self.ewma_jitter_ms;
        }
        let mean_t = self.arrival_at.iter().map(|&v| v as f64).sum::<f64>() / count as f64;
        let mean_o = self.arrival_offset.iter().map(|&v| v as f64).sum::<f64>() / count as f64;
        let mut num = 0.0;
        let mut den = 0.0;
        for (&t, &o) in self.arrival_at.iter().zip(self.arrival_offset.iter()) {
            let dt = t as f64 - mean_t;
            num += dt * (o as f64 - mean_o);
            den += dt * dt;
        }
        let slope = if den > 0.0 { num / den } else { 0.0 };
        let mut residuals: Vec<f64> = self
            .arrival_at
            .iter()
            .zip(self.arrival_offset.iter())
            .map(|(&t, &o)| o as f64 - slope * (t as f64 - mean_t))
            .collect();
        let min_r = residuals.iter().copied().fold(f64::INFINITY, f64::min);
        for r in &mut residuals {
            *r -= min_r;
        }
        residuals.sort_by(f64::total_cmp);
        residuals[(count * 95 / 100).min(count - 1)]
    }

    pub fn snapshot(&mut self, stream_id: u32, buffer_ms: u16, now_ms: u64) -> VoiceStats {
        self.prune(now_ms);
        let total = self.window_events.len();
        let lost = self.window_events.iter().filter(|(_, rx)| !*rx).count();
        let loss_permille = if total == 0 {
            0
        } else {
            ((lost * 1000) / total).min(1000) as u16
        };
        let max_burst = self.burst_events.iter().map(|(_, b)| *b).max().unwrap_or(0);
        let jitter_ms = self.p95_jitter_ms().max(self.ewma_jitter_ms).round().clamp(0.0, 2000.0) as u16;
        VoiceStats {
            stream_id,
            loss_permille,
            jitter_ms,
            buffer_ms,
            max_burst_loss: max_burst,
            flags: 0,
        }
    }
}

// ============================================================================
// WSOLA Pitch-Preserving Time Compression (for seamless backlog convergence)
// ============================================================================

/// Compresses a 20 ms speech frame by ~10-15% without pitch shift or clicks via WSOLA cross-correlation.
pub fn wsola_compress(input: &[i16], target_len: usize) -> Vec<i16> {
    if target_len == 0 || target_len >= input.len() || input.len() < 64 {
        return input.to_vec();
    }
    let cut = input.len() - target_len;
    let overlap = (target_len / 3).clamp(32, input.len() / 2);
    let max_shift = (input.len() - overlap).min(cut + overlap / 2);
    let min_shift = cut.saturating_sub(overlap / 3).max(1);
    if min_shift >= max_shift {
        return input[..target_len].to_vec();
    }
    // Search for the best waveform match around `cut` so the splice has continuous phase.
    let anchor = (target_len - overlap) / 2;
    let mut best_shift = cut.clamp(min_shift, max_shift);
    let mut best_score = i64::MIN;
    let step = if overlap > 120 { 2 } else { 1 };
    for shift in min_shift..=max_shift {
        if anchor + shift + overlap > input.len() {
            break;
        }
        let mut corr = 0i64;
        let mut i = 0;
        while i < overlap {
            corr += (input[anchor + i] as i64) * (input[anchor + shift + i] as i64);
            i += step;
        }
        if corr > best_score {
            best_score = corr;
            best_shift = shift;
        }
    }
    let out_len = input.len() - best_shift;
    let mut out = Vec::with_capacity(out_len);
    out.extend_from_slice(&input[..anchor]);
    for i in 0..overlap {
        let w = (i as f32) / (overlap as f32);
        let a = input[anchor + i] as f32;
        let b = input[anchor + best_shift + i] as f32;
        out.push((a * (1.0 - w) + b * w).round().clamp(-32768.0, 32767.0) as i16);
    }
    out.extend_from_slice(&input[anchor + best_shift + overlap..]);
    out
}

/// 5 ms raised-cosine crossfade between previous tail and new frame head (used after fast-forward jumps).
pub fn crossfade_head(prev_tail: &[i16], frame: &mut [i16]) {
    let n = prev_tail.len().min(frame.len());
    if n == 0 {
        return;
    }
    for i in 0..n {
        let w = 0.5 * (1.0 - (std::f32::consts::PI * (i as f32) / (n as f32)).cos());
        let mixed = prev_tail[i] as f32 * (1.0 - w) + frame[i] as f32 * w;
        frame[i] = mixed.round().clamp(-32768.0, 32767.0) as i16;
    }
}

// ============================================================================
// Adaptive Speech Jitter Buffer (Reordering + RED + LBRR + PLC + DTX + WSOLA)
// ============================================================================

struct BufferedFrame {
    data: Vec<u8>,
    flags: u8,
}

pub struct VoiceJitterBuffer {
    stream_id: u32,
    sample_rate: u32,
    decoder: VoiceOpusDecoder,
    slots: BTreeMap<u32, BufferedFrame>,
    playout_seq: Option<u32>,
    highest_seq: u32,
    started: bool,
    first_arrival_ms: Option<u64>,
    turn_gen: u8,
    turn_gen_locked: bool,
    target_frames: u32,
    underrun_penalty_ms: f64,
    last_decay_ms: u64,
    consecutive_plc: u32,
    expand_credits: u32,
    in_dtx: bool,
    last_tail: Vec<i16>,
    rx_stats: VoiceRxStats,
    scratch: Vec<i16>,
    pub plc_frames: u64,
    pub red_recovered: u64,
    pub lbrr_recovered: u64,
}

impl VoiceJitterBuffer {
    pub fn new(stream_id: u32, sample_rate: u32) -> Result<Self, String> {
        Ok(Self {
            stream_id,
            sample_rate,
            decoder: VoiceOpusDecoder::new(sample_rate)?,
            slots: BTreeMap::new(),
            playout_seq: None,
            highest_seq: 0,
            started: false,
            first_arrival_ms: None,
            turn_gen: 0,
            turn_gen_locked: false,
            target_frames: 1,
            underrun_penalty_ms: 0.0,
            last_decay_ms: 0,
            consecutive_plc: 0,
            expand_credits: 0,
            in_dtx: false,
            last_tail: Vec::new(),
            rx_stats: VoiceRxStats::default(),
            scratch: Vec::new(),
            plc_frames: 0,
            red_recovered: 0,
            lbrr_recovered: 0,
        })
    }

    pub fn clear(&mut self, next_turn_gen: Option<u8>) {
        self.slots.clear();
        self.playout_seq = None;
        self.started = false;
        self.first_arrival_ms = None;
        self.consecutive_plc = 0;
        self.expand_credits = 0;
        self.in_dtx = false;
        self.last_tail.clear();
        self.decoder.reset();
        self.rx_stats.reset_seq_cursor();
        if let Some(g) = next_turn_gen {
            self.turn_gen = g & 0x0F;
            self.turn_gen_locked = true;
        }
    }

    pub fn push(&mut self, pkt: &VoiceOpusPacket, now_ms: u64) {
        let pkt_gen = voice_flags::turn_gen(pkt.flags);
        if self.turn_gen_locked && pkt_gen != self.turn_gen {
            let ahead = pkt_gen.wrapping_sub(self.turn_gen) & 0x0F;
            if ahead == 0 || ahead >= 8 {
                // Strictly behind current turn generation (late pre-barge-in packet): drop!
                return;
            }
            self.clear(Some(pkt_gen));
        } else if !self.turn_gen_locked {
            self.turn_gen = pkt_gen;
            self.turn_gen_locked = true;
        } else if (pkt.flags & voice_flags::RESET) != 0 && self.started {
            // Same generation with RESET only occurs on initial packet; ignore duplicates.
        }

        let is_dtx_pkt = (pkt.flags & voice_flags::DTX) != 0;
        let is_reset_pkt = (pkt.flags & voice_flags::RESET) != 0;
        self.rx_stats.on_primary(pkt.seq, now_ms, is_dtx_pkt || is_reset_pkt || self.in_dtx);
        if self.first_arrival_ms.is_none() {
            self.first_arrival_ms = Some(now_ms);
        }

        // Self-heal if playout_seq drifted ahead during DTX or sustained underrun while empty.
        if self.slots.is_empty() {
            if let Some(p) = self.playout_seq {
                let behind = pkt.seq.wrapping_sub(p) as i32;
                if (self.in_dtx && !is_dtx_pkt) || ((-64..0).contains(&behind) && (self.in_dtx || self.consecutive_plc >= 2)) {
                    self.playout_seq = Some(pkt.seq);
                    self.highest_seq = pkt.seq;
                    self.in_dtx = false;
                    self.consecutive_plc = 0;
                }
            }
        }

        // Insert primary packet if not older than playout cursor.
        let accept_seq = |seq: u32, playout: Option<u32>| -> bool {
            match playout {
                None => true,
                Some(p) => (seq.wrapping_sub(p) as i32) >= 0,
            }
        };

        if accept_seq(pkt.seq, self.playout_seq) {
            self.slots.entry(pkt.seq).or_insert_with(|| BufferedFrame {
                data: pkt.primary.clone(),
                flags: pkt.flags,
            });
            if self.playout_seq.is_none() {
                self.playout_seq = Some(pkt.seq);
                self.highest_seq = pkt.seq;
            } else if (pkt.seq.wrapping_sub(self.highest_seq) as i32) > 0 {
                self.highest_seq = pkt.seq;
            }
        }

        // Recover any missing earlier frames via piggybacked RED entries!
        for red in &pkt.red {
            let r_seq = pkt.seq.wrapping_sub(red.delta_seq as u32);
            if accept_seq(r_seq, self.playout_seq) && !self.slots.contains_key(&r_seq) {
                self.slots.insert(
                    r_seq,
                    BufferedFrame {
                        data: red.data.clone(),
                        flags: voice_flags::FEC | voice_flags::SPEECH,
                    },
                );
                self.red_recovered += 1;
                if let Some(p) = self.playout_seq {
                    if (r_seq.wrapping_sub(p) as i32) < 0 {
                        // unreachable due to accept_seq
                    }
                } else {
                    self.playout_seq = Some(r_seq);
                }
            }
        }

        // Prevent unbounded memory growth under pathological sequence jumps.
        while self.slots.len() > 64 {
            if let Some((&oldest, _)) = self.slots.iter().next() {
                self.slots.remove(&oldest);
            }
        }
        self.update_target(now_ms);
    }

    fn update_target(&mut self, now_ms: u64) {
        if self.last_decay_ms > 0 && now_ms > self.last_decay_ms {
            let elapsed_s = (now_ms - self.last_decay_ms) as f64 / 1000.0;
            self.underrun_penalty_ms = (self.underrun_penalty_ms - elapsed_s * 2.0).max(0.0);
        }
        self.last_decay_ms = now_ms;

        let stats = self.rx_stats.snapshot(self.stream_id, self.buffer_ms(), now_ms);
        let p95 = self.rx_stats.p95_jitter_ms();
        // Keep floor at 1 frame (20 ms) on clean LAN/Wi-Fi; raise to 2 frames (40 ms) under loss
        // so RED-1 / Opus LBRR from seq+1 arrives before playout pops seq!
        let loss_floor_ms = if stats.loss_permille >= 150 || stats.max_burst_loss >= 3 {
            60.0
        } else if stats.loss_permille >= 20 || stats.max_burst_loss >= 1 {
            40.0
        } else {
            20.0
        };
        let prev_target = self.target_frames;
        let desired_ms = (p95 + 15.0 + self.underrun_penalty_ms).clamp(loss_floor_ms, 180.0);
        self.target_frames = ((desired_ms / FRAME_MS as f64).ceil() as u32).clamp(1, 9);
        if self.target_frames > prev_target {
            self.expand_credits = (self.expand_credits + (self.target_frames - prev_target)).min(4);
        }
    }

    pub fn queued_frames(&self) -> u32 {
        if self.slots.is_empty() {
            return 0;
        }
        let Some(p) = self.playout_seq else {
            return 0;
        };
        let span = self.highest_seq.wrapping_sub(p) as i32;
        if span < 0 {
            0
        } else {
            (span as u32 + 1).min(self.slots.len() as u32 + 4)
        }
    }

    pub fn buffer_ms(&self) -> u16 {
        (self.queued_frames() * FRAME_MS).min(u16::MAX as u32) as u16
    }

    pub fn stats_snapshot(&mut self, now_ms: u64) -> VoiceStats {
        let buf = self.buffer_ms();
        self.rx_stats.snapshot(self.stream_id, buf, now_ms)
    }

    /// Pulls the next playout PCM frame (normally 20 ms, or WSOLA-compressed ~17 ms when converging backlog).
    /// Returns `None` only while still waiting for initial pre-roll.
    pub fn pop_frame(&mut self, now_ms: u64) -> Option<Vec<i16>> {
        let p = self.playout_seq?;
        if !self.started {
            let waited = self.first_arrival_ms.map_or(0, |t| now_ms.saturating_sub(t));
            let ready = self.slots.len() as u32 >= self.target_frames || waited >= 100;
            if !ready {
                return None;
            }
            self.started = true;
        }

        // Fast-forward extreme bursts (> target + 180 ms) with 5 ms crossfade to avoid stale multi-second lag.
        let mut jumped = false;
        let span = self.highest_seq.wrapping_sub(p) as i32;
        let max_span = (self.target_frames + 9) as i32;
        let cur_seq = if span > max_span {
            let jump_to = self.highest_seq.wrapping_sub(self.target_frames);
            self.slots.retain(|&k, _| (k.wrapping_sub(jump_to) as i32) >= 0);
            self.playout_seq = Some(jump_to);
            jumped = true;
            jump_to
        } else {
            p
        };

        let frame_len = self.decoder.frame_len();
        let mut pcm = Vec::with_capacity(frame_len);
        let mut is_silence = false;
        let mut advance_seq = true;

        if let Some(entry) = self.slots.remove(&cur_seq) {
            self.consecutive_plc = 0;
            if (entry.flags & voice_flags::DTX) != 0 && entry.data.is_empty() {
                self.in_dtx = true;
                pcm.resize(frame_len, 0);
                is_silence = true;
            } else {
                self.in_dtx = false;
                if self.decoder.decode(&entry.data, &mut self.scratch, false).is_ok() {
                    pcm.extend_from_slice(&self.scratch);
                    is_silence = pcm_rms(&pcm) < SILENCE_RMS_THRESHOLD;
                } else {
                    pcm.resize(frame_len, 0);
                    is_silence = true;
                }
            }
        } else if let Some(next_entry) = self.slots.get(&cur_seq.wrapping_add(1)) {
            if (next_entry.flags & voice_flags::DTX) == 0 && !next_entry.data.is_empty() {
                // Recover missing frame using next frame's embedded Opus LBRR (in-band FEC)!
                self.consecutive_plc = 0;
                self.in_dtx = false;
                let next_bytes = next_entry.data.clone();
                if self.decoder.decode(&next_bytes, &mut self.scratch, true).is_ok() {
                    pcm.extend_from_slice(&self.scratch);
                    self.lbrr_recovered += 1;
                } else if self.decoder.decode_plc(&mut self.scratch).is_ok() {
                    pcm.extend_from_slice(&self.scratch);
                    self.plc_frames += 1;
                } else {
                    pcm.resize(frame_len, 0);
                }
            } else {
                self.in_dtx = true;
                pcm.resize(frame_len, 0);
                is_silence = true;
            }
        } else if self.in_dtx {
            // In active DTX silence interval: emit clean silence without letting playout_seq outrun sender.
            pcm.resize(frame_len, 0);
            is_silence = true;
            if self.slots.is_empty() {
                advance_seq = false;
            }
        } else {
            // Genuine packet loss or jitter underrun: run Opus native PLC with smooth fadeout past 100 ms.
            self.consecutive_plc = self.consecutive_plc.saturating_add(1);
            if self.consecutive_plc == 1 {
                self.underrun_penalty_ms = (self.underrun_penalty_ms + 20.0).min(120.0);
                self.update_target(now_ms);
            }
            if self.consecutive_plc <= MAX_CONSECUTIVE_PLC {
                if self.decoder.decode_plc(&mut self.scratch).is_ok() {
                    pcm.extend_from_slice(&self.scratch);
                    self.plc_frames += 1;
                    // Gentle attenuation on frames 3..5 so prolonged loss never buzzes.
                    if self.consecutive_plc >= 3 {
                        let gain = 1.0 - (self.consecutive_plc - 2) as f32 * 0.28;
                        for s in &mut pcm {
                            *s = (*s as f32 * gain.max(0.0)) as i16;
                        }
                    }
                } else {
                    pcm.resize(frame_len, 0);
                }
                // NetEQ expand: while no newer frame has arrived, hold `playout_seq` at `cur_seq` so
                // slight clock drift or delayed RED packets are never rejected as late.
                if self.slots.is_empty() {
                    advance_seq = false;
                    self.expand_credits = self.expand_credits.saturating_sub(1);
                }
            } else {
                // Past 100 ms with zero packets: clean idle between utterances so next talkspurt starts fast.
                if self.slots.is_empty() {
                    self.playout_seq = None;
                    self.started = false;
                    self.first_arrival_ms = None;
                    self.consecutive_plc = 0;
                    self.underrun_penalty_ms = 0.0;
                    self.rx_stats.reset_timing();
                    return None;
                }
                pcm.resize(frame_len, 0);
                is_silence = true;
            }
        }

        if advance_seq {
            self.playout_seq = Some(cur_seq.wrapping_add(1));
        }

        // Latency convergence: if backlog exceeds target + 2 frames, drop silence or gently WSOLA-compress speech.
        let excess = (self.highest_seq.wrapping_sub(cur_seq) as i32) - self.target_frames as i32;
        if excess >= 3 {
            if is_silence && !self.slots.is_empty() {
                // Recurse once to seamlessly skip a silent pause frame and pull playout closer to live!
                return self.pop_frame(now_ms);
            } else if !is_silence {
                let target_samples = (pcm.len() * 90) / 100; // ~10% pitch-preserving time compression
                pcm = wsola_compress(&pcm, target_samples);
            }
        }

        if jumped && !self.last_tail.is_empty() {
            crossfade_head(&self.last_tail, &mut pcm);
        }

        // Save 5 ms tail for potential crossfade.
        let tail_len = ((self.sample_rate as usize) * 5 / 1000).min(pcm.len());
        self.last_tail.clear();
        if tail_len > 0 {
            self.last_tail.extend_from_slice(&pcm[pcm.len() - tail_len..]);
        }
        Some(pcm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_frame(rate: u32, freq_hz: f32, frame_idx: u32) -> Vec<i16> {
        let n = frame_samples(rate);
        let start = frame_idx as usize * n;
        (0..n)
            .map(|i| {
                let t = (start + i) as f32 / rate as f32;
                ((t * freq_hz * std::f32::consts::TAU).sin() * 14000.0) as i16
            })
            .collect()
    }

    #[test]
    fn opus_red_and_lbrr_survive_30_percent_loss_and_bursts() {
        let rate = 16_000;
        let mut enc = VoiceOpusEncoder::new(rate, 24_000, false).unwrap();
        let mut red_hist = RedHistory::default();
        let mut jb = VoiceJitterBuffer::new(1, rate).unwrap();

        // Simulate 60 frames (1.2 s) with 30% packet loss INCLUDING a 3-packet (60 ms) consecutive burst
        // and out-of-order delivery, with RED-2 active.
        let mut sent_pkts = Vec::new();
        for seq in 0..60u32 {
            let pcm = sine_frame(rate, 440.0, seq);
            let encoded = enc.encode(&pcm).unwrap();
            let red = red_hist.build_red(seq, 2);
            red_hist.record(seq, encoded.clone());
            sent_pkts.push(VoiceOpusPacket {
                stream_id: 1,
                seq,
                ts_samples: seq * frame_samples(rate) as u32,
                flags: voice_flags::FEC | voice_flags::SPEECH,
                red,
                primary: encoded,
            });
        }

        // Drop burst seq 10, 11, 12 (60 ms burst!) + periodic ~25% loss + reorder seq 25/24
        let dropped = |s: u32| matches!(s, 10 | 11 | 12 | 5 | 18 | 21 | 29 | 33 | 37 | 41 | 45 | 50 | 54);
        let mut ordered_deliver: Vec<(u64, VoiceOpusPacket)> = Vec::new();
        for pkt in sent_pkts {
            if dropped(pkt.seq) {
                continue;
            }
            let at = (pkt.seq as u64) * 20 + if pkt.seq == 24 { 25 } else { 0 };
            ordered_deliver.push((at, pkt));
        }
        ordered_deliver.sort_by_key(|(t, _)| *t);

        let mut deliver_idx = 0usize;
        let mut played_frames = 0u32;
        let mut non_silent = 0u32;
        for tick in 0..65u64 {
            let now = tick * 20;
            while deliver_idx < ordered_deliver.len() && ordered_deliver[deliver_idx].0 <= now {
                jb.push(&ordered_deliver[deliver_idx].1, now);
                deliver_idx += 1;
            }
            if let Some(pcm) = jb.pop_frame(now) {
                played_frames += 1;
                if pcm_rms(&pcm) > 0.05 {
                    non_silent += 1;
                }
            }
        }
        assert!(played_frames >= 55, "played={played_frames}");
        assert!(non_silent >= 50, "audio stayed continuous through 60ms burst + 25% loss (non_silent={non_silent})");
        assert!(jb.red_recovered >= 8, "RED recovered lost frames: {}", jb.red_recovered);
    }

    #[test]
    fn link_controller_escalates_fast_and_steps_down_with_hysteresis() {
        let mut ctrl = VoiceLinkController::for_sample_rate(24_000);
        assert_eq!(ctrl.profile().red_count, 0);
        assert_eq!(ctrl.profile().bitrate, 36_000);

        // Immediate jump on 20% loss + 3-frame burst
        let bad = VoiceStats { stream_id: 1, loss_permille: 220, jitter_ms: 80, buffer_ms: 100, max_burst_loss: 3, flags: 0 };
        let p = ctrl.on_stats(&bad, 1000);
        assert_eq!(p.red_count, 3);
        assert!(p.bitrate <= 18_000);
        assert_eq!(p.opus_loss_perc, 30);

        // Clean reports do not flap down immediately
        let clean = VoiceStats { stream_id: 1, loss_permille: 0, jitter_ms: 5, buffer_ms: 20, max_burst_loss: 0, flags: 0 };
        for i in 1..=8 {
            ctrl.on_stats(&clean, 1000 + i * 250);
        }
        assert!(ctrl.profile().red_count >= 2, "must not flap down within 2s");

        // After sustained clean period, steps all the way back down to 0 RED / full bitrate
        for i in 9..=60 {
            ctrl.on_stats(&clean, 1000 + i * 250);
        }
        assert_eq!(ctrl.profile().red_count, 0);
        assert_eq!(ctrl.profile().bitrate, 36_000);
    }

    #[test]
    fn wsola_compress_shortens_frame_smoothly() {
        let pcm = sine_frame(24_000, 300.0, 0);
        assert_eq!(pcm.len(), 480);
        let compressed = wsola_compress(&pcm, 420);
        assert!(compressed.len() < 480 && compressed.len() >= 380, "len={}", compressed.len());
        assert!(pcm_rms(&compressed) > 0.2, "energy preserved");
    }

    #[test]
    fn turn_gen_drops_stale_pre_barge_in_packets() {
        let mut enc = VoiceOpusEncoder::new(24_000, 32_000, false).unwrap();
        let mut jb = VoiceJitterBuffer::new(1, 24_000).unwrap();
        let frame = enc.encode(&sine_frame(24_000, 440.0, 0)).unwrap();

        jb.push(
            &VoiceOpusPacket {
                stream_id: 1,
                seq: 1,
                ts_samples: 480,
                flags: voice_flags::with_gen(voice_flags::SPEECH, 1),
                red: vec![],
                primary: frame.clone(),
            },
            0,
        );
        assert!(jb.pop_frame(20).is_some());

        // Barge-in increments turn_gen to 2
        jb.clear(Some(2));
        // Late stale gen=1 packet arrives after barge-in -> MUST be ignored
        jb.push(
            &VoiceOpusPacket {
                stream_id: 1,
                seq: 2,
                ts_samples: 960,
                flags: voice_flags::with_gen(voice_flags::SPEECH, 1),
                red: vec![],
                primary: frame.clone(),
            },
            40,
        );
        assert!(jb.pop_frame(60).is_none(), "stale pre-barge-in packet must be ignored");

        // Fresh gen=2 packet with RESET arrives -> accepted cleanly
        jb.push(
            &VoiceOpusPacket {
                stream_id: 1,
                seq: 3,
                ts_samples: 1440,
                flags: voice_flags::with_gen(voice_flags::SPEECH | voice_flags::RESET, 2),
                red: vec![],
                primary: frame,
            },
            80,
        );
        assert!(jb.pop_frame(100).is_some());
    }

    #[test]
    fn clock_drift_and_dtx_pause_never_drop_resumed_speech() {
        let rate = 16_000;
        let mut enc = VoiceOpusEncoder::new(rate, 28_000, false).unwrap();
        let mut jb = VoiceJitterBuffer::new(1, rate).unwrap();

        // 1. Steady 15 frames of speech
        for seq in 0..15u32 {
            let encoded = enc.encode(&sine_frame(rate, 440.0, seq)).unwrap();
            jb.push(
                &VoiceOpusPacket {
                    stream_id: 1,
                    seq,
                    ts_samples: seq * frame_samples(rate) as u32,
                    flags: voice_flags::FEC | voice_flags::SPEECH,
                    red: vec![],
                    primary: encoded,
                },
                seq as u64 * 20,
            );
            assert!(jb.pop_frame(seq as u64 * 20).is_some());
        }

        // 2. Simulate 400 ms sender pause (or clock drift where receiver ticks 20 times with no packets)
        for t in 15..35u64 {
            let _ = jb.pop_frame(t * 20);
        }

        // 3. Sender resumes at seq = 15 (even though receiver ticked 20 times during the gap!)
        let mut audible = 0u32;
        for i in 0..15u32 {
            let seq = 15 + i;
            let now = (35 + i as u64) * 20;
            let encoded = enc.encode(&sine_frame(rate, 440.0, seq)).unwrap();
            jb.push(
                &VoiceOpusPacket {
                    stream_id: 1,
                    seq,
                    ts_samples: seq * frame_samples(rate) as u32,
                    flags: voice_flags::FEC | voice_flags::SPEECH,
                    red: vec![],
                    primary: encoded,
                },
                now,
            );
            if let Some(pcm) = jb.pop_frame(now) {
                if pcm_rms(&pcm) > 0.05 {
                    audible += 1;
                }
            }
        }
        assert!(audible >= 13, "resumed speech after clock drift / pause must not be dropped (audible={audible}/15)");
        assert_eq!(jb.stats_snapshot(50 * 20).loss_permille, 0, "pause without gaps must report 0% loss");
    }
}
