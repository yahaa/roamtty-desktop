//! The UDP transport (PROTOCOL.md "UDP"): every datagram is `u8 dir | u64 seq` in the clear, then
//! AES-128-GCM of one packet with that header as associated data. The nonce is `dir, 0, 0, 0` and
//! the big-endian sequence number, so a key never sees a nonce twice; the sequence also rejects
//! replays. Protocol frames travel as messages cut into equal shards plus Reed-Solomon parity.

use crate::fec;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes128Gcm, Nonce};
use std::collections::{BTreeMap, HashMap};

pub const DIR_C2S: u8 = 0;
pub const DIR_S2C: u8 = 1;
pub const HEADER: usize = 9;
pub const TAG: usize = 16;
/// Shard bytes per datagram: with the FRAG header and AES-GCM overhead a datagram stays near
/// 1200 bytes, under any path MTU of 1280 (IPv6 minimum, Tailscale).
pub const SHARD: usize = 1150;
/// Data shards per FEC block; a bigger message is cut into several blocks.
pub const MAX_K: usize = 160;

pub mod kind {
    pub const FRAG: u8 = 1;
    pub const RACK: u8 = 2;
    pub const HEARTBEAT: u8 = 3;
    pub const FEEDBACK: u8 = 4;
    pub const NACK: u8 = 5;
}

pub const FLAG_RELIABLE: u8 = 1;
pub const FLAG_VIDEO: u8 = 2;

pub struct Crypto { cipher: Aes128Gcm }

impl Crypto {
    pub fn new(key: &[u8; 16]) -> Crypto { Crypto { cipher: Aes128Gcm::new(key.into()) } }

    fn nonce(dir: u8, seq: u64) -> [u8; 12] {
        let mut n = [0u8; 12];
        n[0] = dir;
        n[4..].copy_from_slice(&seq.to_be_bytes());
        n
    }

    pub fn seal(&self, dir: u8, seq: u64, plain: &[u8]) -> Vec<u8> {
        let mut head = [0u8; HEADER];
        head[0] = dir;
        head[1..].copy_from_slice(&seq.to_be_bytes());
        let nonce = Self::nonce(dir, seq);
        let sealed = self.cipher.encrypt(Nonce::from_slice(&nonce), Payload { msg: plain, aad: &head }).expect("seal");
        let mut out = Vec::with_capacity(HEADER + sealed.len());
        out.extend_from_slice(&head);
        out.extend_from_slice(&sealed);
        out
    }

    /// (dir, seq, plaintext), or None for anything not sealed with this key.
    pub fn open(&self, packet: &[u8]) -> Option<(u8, u64, Vec<u8>)> {
        if packet.len() < HEADER + TAG { return None; }
        let dir = packet[0];
        let seq = u64::from_be_bytes(packet[1..HEADER].try_into().ok()?);
        let nonce = Self::nonce(dir, seq);
        let plain = self.cipher.decrypt(Nonce::from_slice(&nonce), Payload { msg: &packet[HEADER..], aad: &packet[..HEADER] }).ok()?;
        Some((dir, seq, plain))
    }
}

/// Remembers the last 1024 sequence numbers seen, so a replayed datagram is dropped.
#[derive(Default)]
pub struct ReplayWindow { top: u64, bits: Vec<u64> }

impl ReplayWindow {
    pub fn accept(&mut self, seq: u64) -> bool {
        const W: u64 = 1024;
        if self.bits.is_empty() { self.bits = vec![0; (W / 64) as usize]; }
        if seq + W <= self.top { return false; }
        if seq > self.top {
            let shift = seq - self.top;
            if shift >= W { self.bits.iter_mut().for_each(|b| *b = 0); }
            else { for s in self.top + 1..=seq { self.bits[((s % W) / 64) as usize] &= !(1 << (s % 64)); } }
            self.top = seq;
        }
        let (w, b) = (((seq % W) / 64) as usize, seq % 64);
        if self.bits[w] & (1 << b) != 0 { return false; }
        self.bits[w] |= 1 << b;
        true
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Frag {
    pub msg: u32,
    pub flags: u8,
    pub block: u8,
    pub blocks: u8,
    pub index: u16,
    pub k: u16,
    pub m: u16,
    /// Bytes of this block's data before padding.
    pub len: u32,
    pub shard: Vec<u8>,
}

impl Frag {
    /// This shard as a FRAG packet: what `Packet::Frag(f).encode()` gives, without giving up `f`.
    pub fn packet(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(18 + self.shard.len());
        v.push(kind::FRAG);
        v.extend(self.msg.to_be_bytes());
        v.push(self.flags); v.push(self.block); v.push(self.blocks);
        v.extend(self.index.to_be_bytes()); v.extend(self.k.to_be_bytes()); v.extend(self.m.to_be_bytes());
        v.extend(self.len.to_be_bytes());
        v.extend_from_slice(&self.shard);
        v
    }
}

/// What a video message did on the way, reported by the receiver for rate control.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub msg: u32,
    pub received: u16,
    pub total: u16,
    pub first_us: u64,
    pub last_us: u64,
    /// 0 lost, 1 rebuilt without FEC, 2 rebuilt with parity.
    pub outcome: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Packet {
    Frag(Frag),
    Rack(Vec<u32>),
    /// `t` the sender's clock in µs, `echo` the last `t` it heard from the peer (0 for none).
    Heartbeat { t: u64, echo: u64 },
    Feedback(Vec<Report>),
    Nack { msg: u32, block: u8, indices: Vec<u16> },
}

impl Packet {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::new();
        match self {
            Packet::Frag(f) => return f.packet(),
            Packet::Rack(ids) => {
                v.push(kind::RACK);
                v.extend((ids.len() as u16).to_be_bytes());
                for i in ids { v.extend(i.to_be_bytes()); }
            }
            Packet::Heartbeat { t, echo } => { v.push(kind::HEARTBEAT); v.extend(t.to_be_bytes()); v.extend(echo.to_be_bytes()); }
            Packet::Feedback(rs) => {
                v.push(kind::FEEDBACK);
                v.extend((rs.len() as u16).to_be_bytes());
                for r in rs {
                    v.extend(r.msg.to_be_bytes()); v.extend(r.received.to_be_bytes()); v.extend(r.total.to_be_bytes());
                    v.extend(r.first_us.to_be_bytes()); v.extend(r.last_us.to_be_bytes()); v.push(r.outcome);
                }
            }
            Packet::Nack { msg, block, indices } => {
                v.push(kind::NACK);
                v.extend(msg.to_be_bytes()); v.push(*block);
                v.extend((indices.len() as u16).to_be_bytes());
                for i in indices { v.extend(i.to_be_bytes()); }
            }
        }
        v
    }

    pub fn decode(p: &[u8]) -> Option<Packet> {
        let mut c = R(p);
        Some(match c.u8()? {
            kind::FRAG => {
                let msg = c.u32()?; let flags = c.u8()?; let block = c.u8()?; let blocks = c.u8()?;
                let index = c.u16()?; let k = c.u16()?; let m = c.u16()?; let len = c.u32()?;
                Packet::Frag(Frag { msg, flags, block, blocks, index, k, m, len, shard: c.0.to_vec() })
            }
            kind::RACK => { let n = c.u16()?; Packet::Rack((0..n).map(|_| c.u32()).collect::<Option<_>>()?) }
            kind::HEARTBEAT => Packet::Heartbeat { t: c.u64()?, echo: c.u64()? },
            kind::FEEDBACK => {
                let n = c.u16()?;
                Packet::Feedback((0..n).map(|_| Some(Report { msg: c.u32()?, received: c.u16()?, total: c.u16()?,
                    first_us: c.u64()?, last_us: c.u64()?, outcome: c.u8()? })).collect::<Option<_>>()?)
            }
            kind::NACK => {
                let msg = c.u32()?; let block = c.u8()?; let n = c.u16()?;
                Packet::Nack { msg, block, indices: (0..n).map(|_| c.u16()).collect::<Option<_>>()? }
            }
            _ => return None,
        })
    }
}

struct R<'a>(&'a [u8]);
impl R<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> { if self.0.len() < n { return None; } let (a, b) = self.0.split_at(n); self.0 = b; Some(a) }
    fn u8(&mut self) -> Option<u8> { Some(self.take(1)?[0]) }
    fn u16(&mut self) -> Option<u16> { Some(u16::from_be_bytes(self.take(2)?.try_into().ok()?)) }
    fn u32(&mut self) -> Option<u32> { Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?)) }
    fn u64(&mut self) -> Option<u64> { Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?)) }
}

/// Parity shards for `k` data shards at `ratio` (0.05–1.0): at least one whenever ratio > 0.
pub fn parity_for(k: usize, ratio: f32) -> usize {
    if ratio <= 0.0 { return 0; }
    ((k as f32 * ratio).ceil() as usize).max(1).min(fec::MAX_SHARDS - k)
}

/// Cuts one message (a whole protocol frame) into FEC blocks of shards.
pub fn fragment(msg: u32, flags: u8, bytes: &[u8], ratio: f32) -> Vec<Frag> {
    let mut out = Vec::new();
    fragment_each(msg, flags, bytes, ratio, |f| out.push(f.clone()));
    out
}

/// [fragment], handing each shard to `emit` as soon as it exists: a block's data shards before its
/// parity is computed, so they can be on the wire while the Reed-Solomon work goes on.
pub fn fragment_each(msg: u32, flags: u8, bytes: &[u8], ratio: f32, mut emit: impl FnMut(&Frag)) {
    let per_block = SHARD * MAX_K;
    let blocks = bytes.len().div_ceil(per_block).max(1);
    assert!(blocks <= 255, "message too large for UDP");
    for b in 0..blocks {
        let chunk = &bytes[b * per_block..((b + 1) * per_block).min(bytes.len())];
        let k = chunk.len().div_ceil(SHARD).max(1);
        // Equal shards: the block's size spread evenly, so the last one is not mostly padding.
        let size = chunk.len().div_ceil(k).max(1);
        let m = parity_for(k, ratio);
        let frag = |i: usize, shard: Vec<u8>| Frag { msg, flags, block: b as u8, blocks: blocks as u8, index: i as u16, k: k as u16, m: m as u16,
            len: chunk.len() as u32, shard };
        let data: Vec<Frag> = (0..k).map(|i| {
            let mut s = chunk[(i * size).min(chunk.len())..((i + 1) * size).min(chunk.len())].to_vec();
            s.resize(size, 0);
            let f = frag(i, s);
            emit(&f);
            f
        }).collect();
        if m > 0 {
            for (i, shard) in fec::encode(&data.iter().map(|f| f.shard.as_slice()).collect::<Vec<_>>(), m).into_iter().enumerate() {
                emit(&frag(k + i, shard));
            }
        }
    }
}

struct Block { k: usize, len: usize, shards: Vec<Option<Vec<u8>>>, data: Option<Vec<u8>>, parity_used: bool }

struct Pending { flags: u8, blocks: Vec<Option<Block>>, first_us: u64, last_us: u64, received: u16, total: u16 }

/// Puts messages back together. Feed it fragments; complete messages come out once, in any order.
#[derive(Default)]
pub struct Reassembler {
    pending: HashMap<u32, Pending>,
    /// Messages already delivered (or given up), so late shards are ignored.
    done: BTreeMap<u32, ()>,
    /** Finished video messages, kept a moment so parity arriving after the rebuild still counts. */
    settling: Vec<(Report, u64)>,
}

pub const SETTLE_US: u64 = 50_000;

pub struct Delivered { pub msg: u32, pub flags: u8, pub bytes: Vec<u8> }

impl Reassembler {
    pub fn push(&mut self, f: Frag, now_us: u64) -> Option<Delivered> {
        if self.done.contains_key(&f.msg) {
            if let Some((r, _)) = self.settling.iter_mut().find(|(r, _)| r.msg == f.msg) { r.received += 1; }
            return None;
        }
        let k = f.k as usize;
        let n = k + f.m as usize;
        if k == 0 || n > fec::MAX_SHARDS || f.index as usize >= n || f.block >= f.blocks { return None; }
        let p = self.pending.entry(f.msg).or_insert_with(|| Pending {
            flags: f.flags, blocks: (0..f.blocks).map(|_| None).collect(), first_us: now_us, last_us: now_us, received: 0, total: 0,
        });
        if p.blocks.len() != f.blocks as usize { return None; }
        p.last_us = now_us;
        let slot = &mut p.blocks[f.block as usize];
        if slot.is_none() {
            p.total += n as u16;
            *slot = Some(Block { k, len: f.len as usize, shards: vec![None; n], data: None, parity_used: false });
        }
        let b = slot.as_mut().unwrap();
        // A shard of a block already rebuilt still arrived: count it, or rate control sees loss.
        if b.data.is_some() { p.received += 1; return None; }
        if b.shards.len() != n || b.shards[f.index as usize].is_some() { return None; }
        b.shards[f.index as usize] = Some(f.shard);
        p.received += 1;
        if b.shards.iter().filter(|s| s.is_some()).count() >= b.k {
            let parity_used = (0..b.k).any(|i| b.shards[i].is_none());
            let data = fec::decode(&b.shards, b.k)?;
            let mut joined: Vec<u8> = data.concat();
            joined.truncate(b.len);
            b.data = Some(joined);
            b.parity_used = parity_used;
            b.shards.clear();
        }
        if p.blocks.iter().all(|b| b.as_ref().is_some_and(|b| b.data.is_some())) {
            let p = self.pending.remove(&f.msg).unwrap();
            let parity = p.blocks.iter().any(|b| b.as_ref().unwrap().parity_used);
            let bytes: Vec<u8> = p.blocks.into_iter().flat_map(|b| b.unwrap().data.unwrap()).collect();
            self.finish(f.msg, p.flags, p.first_us, p.last_us, p.received, p.total, if parity { 2 } else { 1 }, now_us);
            return Some(Delivered { msg: f.msg, flags: p.flags, bytes });
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    fn finish(&mut self, msg: u32, flags: u8, first: u64, last: u64, received: u16, total: u16, outcome: u8, now_us: u64) {
        self.done.insert(msg, ());
        while self.done.len() > 4096 { let k = *self.done.keys().next().unwrap(); self.done.remove(&k); }
        if flags & FLAG_VIDEO != 0 {
            self.settling.push((Report { msg, received, total, first_us: first, last_us: last, outcome }, now_us));
        }
    }

    /// Whether `msg` was delivered (or given up on): a reliable resend of it only needs its RACK again.
    pub fn is_done(&self, msg: u32) -> bool { self.done.contains_key(&msg) }

    /// Missing shard indices of messages still waiting after `older_than_us`: what a NACK asks for.
    pub fn missing(&self, now_us: u64, older_than_us: u64) -> Vec<(u32, u8, Vec<u16>)> {
        let mut out = Vec::new();
        for (msg, p) in &self.pending {
            if now_us.saturating_sub(p.last_us) < older_than_us { continue; }
            for (bi, b) in p.blocks.iter().enumerate() {
                if let Some(b) = b {
                    if b.data.is_some() { continue; }
                    let have = b.shards.iter().filter(|s| s.is_some()).count();
                    let need = b.k.saturating_sub(have);
                    let idx: Vec<u16> = (0..b.shards.len()).filter(|i| b.shards[*i].is_none()).take(need).map(|i| i as u16).collect();
                    if !idx.is_empty() { out.push((*msg, bi as u8, idx)); }
                }
            }
        }
        out
    }

    /// Gives up on messages silent for `timeout_us`; returns the video ones lost (a keyframe is due).
    pub fn expire(&mut self, now_us: u64, timeout_us: u64) -> Vec<u32> {
        let stale: Vec<u32> = self.pending.iter().filter(|(_, p)| now_us.saturating_sub(p.last_us) > timeout_us).map(|(m, _)| *m).collect();
        let mut lost = Vec::new();
        for m in stale {
            let p = self.pending.remove(&m).unwrap();
            if p.flags & FLAG_VIDEO != 0 { lost.push(m); }
            // A reliable message is only forgotten: the sender resends it until acknowledged, and that
            // resend must start it over rather than be acknowledged unseen.
            if p.flags & FLAG_RELIABLE != 0 { continue; }
            self.finish(m, p.flags, p.first_us, p.last_us, p.received, p.total, 0, now_us);
        }
        lost
    }

    /// Reports of video messages finished at least SETTLE_US before `now_us` (u64::MAX: all).
    pub fn take_reports(&mut self, now_us: u64) -> Vec<Report> {
        let n = self.settling.iter().take_while(|(_, at)| now_us == u64::MAX || now_us.saturating_sub(*at) >= SETTLE_US).count();
        self.settling.drain(..n).map(|(r, _)| r).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_and_reject() {
        let c = Crypto::new(&[7u8; 16]);
        let p = c.seal(DIR_S2C, 42, b"hello");
        assert_eq!(c.open(&p), Some((DIR_S2C, 42, b"hello".to_vec())));
        let mut bad = p.clone();
        bad[0] = DIR_C2S; // header is authenticated
        assert!(c.open(&bad).is_none());
        assert!(Crypto::new(&[8u8; 16]).open(&p).is_none());
    }

    #[test]
    fn replay_window() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(5));
        assert!(!w.accept(5));
        assert!(w.accept(3));
        assert!(w.accept(2000));
        assert!(!w.accept(900));
        assert!(w.accept(1999));
    }

    #[test]
    fn shards_come_out_data_first_and_encode_alike() {
        let bytes: Vec<u8> = (0..SHARD * (MAX_K + 5)).map(|i| i as u8).collect();
        let mut seen = Vec::new();
        fragment_each(4, FLAG_VIDEO, &bytes, 0.3, |f| {
            assert_eq!(f.packet(), Packet::Frag(f.clone()).encode());
            seen.push((f.block, f.index, f.k));
        });
        assert_eq!(seen.len(), fragment(4, FLAG_VIDEO, &bytes, 0.3).len());
        // Within a block every data shard precedes every parity shard, and blocks go in order.
        let firsts: Vec<_> = seen.iter().map(|s| (s.0, s.1 >= s.2)).collect();
        let mut sorted = firsts.clone();
        sorted.sort();
        assert_eq!(firsts, sorted);
    }

    #[test]
    fn fragments_survive_loss() {
        let bytes: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let frags = fragment(9, FLAG_VIDEO, &bytes, 0.2);
        assert!(frags.iter().all(|f| f.shard.len() <= SHARD));
        assert!(frags.iter().map(|f| f.block).max().unwrap() >= 1, "300 KB takes two blocks");
        let mut r = Reassembler::default();
        let mut out = None;
        // Drop every 7th shard: 14% loss, within 20% parity.
        for (i, f) in frags.into_iter().enumerate() {
            if i % 7 == 3 { continue; }
            let enc = Packet::Frag(f).encode();
            let Packet::Frag(f) = Packet::decode(&enc).unwrap() else { panic!() };
            if let Some(d) = r.push(f, i as u64) { out = Some(d); }
        }
        assert_eq!(out.unwrap().bytes, bytes);
        let rep = r.take_reports(u64::MAX);
        assert_eq!(rep.len(), 1);
        assert_eq!(rep[0].outcome, 2);
    }

    #[test]
    fn small_message_and_nack() {
        let frags = fragment(1, FLAG_RELIABLE, b"tiny", 0.0);
        assert_eq!(frags.len(), 1);
        let mut r = Reassembler::default();
        assert_eq!(r.push(frags[0].clone(), 0).unwrap().bytes, b"tiny");
        assert!(r.push(frags[0].clone(), 0).is_none(), "delivered once");
        let big: Vec<u8> = vec![1; SHARD * 3];
        let frags = fragment(2, FLAG_VIDEO, &big, 0.0);
        r.push(frags[0].clone(), 10);
        assert_eq!(r.missing(50_000, 20_000), vec![(2, 0, vec![1, 2])]);
        assert_eq!(r.expire(2_000_000, 1_000_000), vec![2]);
        assert_eq!(r.take_reports(u64::MAX)[0].outcome, 0);
    }

    #[test]
    fn an_expired_reliable_message_can_still_arrive() {
        let mut r = Reassembler::default();
        let frags = fragment(9, FLAG_RELIABLE, &vec![7; SHARD * 3], 0.0);
        assert!(r.push(frags[0].clone(), 0).is_none());
        assert!(!r.is_done(9), "half a message is not done: no RACK for it");
        assert!(r.expire(3_000_000, 2_000_000).is_empty());
        assert!(!r.is_done(9), "forgotten, not given up on");
        let out = frags.iter().filter_map(|f| r.push(f.clone(), 3_000_001)).next();
        assert_eq!(out.unwrap().bytes.len(), SHARD * 3);
        assert!(r.is_done(9));
    }

    #[test]
    fn late_shards_of_a_rebuilt_block_count() {
        let mut r = Reassembler::default();
        let frags = fragment(3, FLAG_VIDEO, &vec![5; SHARD * 2 * MAX_K], 0.25);
        assert!(frags[0].blocks > 1);
        for f in &frags { r.push(f.clone(), 0); }
        let rep = r.take_reports(u64::MAX);
        assert_eq!((rep[0].received as usize, rep[0].total as usize), (frags.len(), frags.len()));
    }

    #[test]
    fn packets_round_trip() {
        for p in [
            Packet::Rack(vec![1, 2, 3]),
            Packet::Heartbeat { t: 5, echo: 6 },
            Packet::Feedback(vec![Report { msg: 1, received: 3, total: 4, first_us: 10, last_us: 20, outcome: 2 }]),
            Packet::Nack { msg: 3, block: 0, indices: vec![4, 5] },
        ] {
            assert_eq!(Packet::decode(&p.encode()), Some(p));
        }
    }
}
