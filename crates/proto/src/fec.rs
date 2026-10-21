//! Reed-Solomon erasure code over GF(2^8) (polynomial 0x11d) with a Cauchy matrix: k data shards
//! and m parity shards, any k of them rebuild the data. The apps implement the same matrix, so
//! parity made here decodes there (golden samples in testdata/).

const POLY: u16 = 0x11d;

struct Tables { exp: [u8; 512], log: [u8; 256] }

fn tables() -> &'static Tables {
    static T: std::sync::OnceLock<Tables> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let mut exp = [0u8; 512];
        let mut log = [0u8; 256];
        let mut x: u16 = 1;
        for i in 0..255 {
            exp[i] = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 { x ^= POLY; }
        }
        for i in 255..512 { exp[i] = exp[i - 255]; }
        Tables { exp, log }
    })
}

pub fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 { return 0; }
    let t = tables();
    t.exp[t.log[a as usize] as usize + t.log[b as usize] as usize]
}

pub fn inv(a: u8) -> u8 {
    assert!(a != 0);
    let t = tables();
    t.exp[255 - t.log[a as usize] as usize]
}

/// Row `j` of the parity matrix: C[j][i] = 1 / (x_j + y_i) with y_i = i and x_j = k + j.
fn cauchy(k: usize, j: usize, i: usize) -> u8 { inv(((k + j) as u8) ^ (i as u8)) }

pub const MAX_SHARDS: usize = 255;

/// Parity shards for equally sized `data` shards.
pub fn encode(data: &[&[u8]], m: usize) -> Vec<Vec<u8>> {
    let k = data.len();
    assert!(k + m <= MAX_SHARDS && k > 0);
    let len = data[0].len();
    (0..m).map(|j| {
        let mut p = vec![0u8; len];
        for (i, d) in data.iter().enumerate() {
            let c = cauchy(k, j, i);
            for (o, b) in p.iter_mut().zip(d.iter()) { *o ^= mul(c, *b); }
        }
        p
    }).collect()
}

/// Rebuilds the k data shards from any k shards; `shards[i]` is shard i (data first, then parity)
/// or None when lost. Returns None with fewer than k shards present.
pub fn decode(shards: &[Option<Vec<u8>>], k: usize) -> Option<Vec<Vec<u8>>> {
    let present: Vec<usize> = (0..shards.len()).filter(|i| shards[*i].is_some()).take(k).collect();
    if present.len() < k { return None; }
    if present.iter().all(|i| *i < k) && present.len() == k && (0..k).all(|i| shards[i].is_some()) {
        return Some((0..k).map(|i| shards[i].clone().unwrap()).collect());
    }
    let len = shards[present[0]].as_ref().unwrap().len();
    // Rows of the encoding matrix for the shards we have, inverted.
    let mut a: Vec<Vec<u8>> = present.iter().map(|&s| {
        (0..k).map(|i| if s < k { (s == i) as u8 } else { cauchy(k, s - k, i) }).collect()
    }).collect();
    let mut b: Vec<Vec<u8>> = (0..k).map(|r| (0..k).map(|c| (r == c) as u8).collect()).collect();
    for col in 0..k {
        let pivot = (col..k).find(|r| a[*r][col] != 0)?;
        a.swap(col, pivot);
        b.swap(col, pivot);
        let f = inv(a[col][col]);
        for c in 0..k { a[col][c] = mul(a[col][c], f); b[col][c] = mul(b[col][c], f); }
        for r in 0..k {
            if r != col && a[r][col] != 0 {
                let f = a[r][col];
                for c in 0..k {
                    a[r][c] ^= mul(f, a[col][c]);
                    b[r][c] ^= mul(f, b[col][c]);
                }
            }
        }
    }
    Some((0..k).map(|i| {
        let mut out = vec![0u8; len];
        for (r, &s) in present.iter().enumerate() {
            let c = b[i][r];
            if c == 0 { continue; }
            for (o, v) in out.iter_mut().zip(shards[s].as_ref().unwrap().iter()) { *o ^= mul(c, *v); }
        }
        out
    }).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shards(k: usize, len: usize, seed: u32) -> Vec<Vec<u8>> {
        let mut x = seed;
        (0..k).map(|_| (0..len).map(|_| { x = x.wrapping_mul(1103515245).wrapping_add(12345); (x >> 16) as u8 }).collect()).collect()
    }

    #[test]
    fn field() {
        for a in 1..=255u8 { assert_eq!(mul(a, inv(a)), 1); }
        assert_eq!(mul(2, 0x80), 0x1d);
        assert_eq!(mul(3, 7), 9);
    }

    #[test]
    fn any_k_rebuild() {
        for (k, m) in [(1, 1), (4, 2), (10, 3), (60, 18), (200, 55)] {
            let data = shards(k, 37, k as u32);
            let refs: Vec<&[u8]> = data.iter().map(|v| v.as_slice()).collect();
            let parity = encode(&refs, m);
            let mut all: Vec<Option<Vec<u8>>> = data.iter().cloned().chain(parity).map(Some).collect();
            // Lose the first m data shards: only parity can stand in.
            for s in all.iter_mut().take(m.min(k)) { *s = None; }
            assert_eq!(decode(&all, k).unwrap(), data, "k={k} m={m}");
            all[k + m - 1] = None;
            assert!(decode(&all, k).is_none(), "k-1 shards cannot rebuild");
        }
    }
}
