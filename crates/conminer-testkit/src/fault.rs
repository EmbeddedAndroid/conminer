//! Fault injection (§12.3).
//!
//! Serial input is hostile by definition: a wrong baud rate produces structured
//! garbage, a marginal cable flips bits, a board yanked mid-oops truncates a
//! record. These transforms sit between the corpus and the pty so the identical
//! live pipeline meets all of it — deterministically, because a flaky
//! fault-injection test is worse than none.

/// Deterministic pseudo-random stream. Not cryptographic and not meant to be:
/// the point is that a failing seed reproduces exactly.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // Mix before forcing the low bit, or adjacent seeds (42 and 43) collapse
        // to the same stream and a "different seed" test silently passes.
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }

    pub fn chance(&mut self, one_in: u64) -> bool {
        one_in > 0 && self.next_u64() % one_in == 0
    }
}

/// Flip individual bits, as line noise does.
pub fn bit_flips(data: &[u8], seed: u64, one_in: u64) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    data.iter()
        .map(|&b| {
            if rng.chance(one_in) {
                b ^ (1u8 << rng.below(8))
            } else {
                b
            }
        })
        .collect()
}

/// Splice in a burst of high-entropy bytes, as a baud mismatch does.
pub fn garbage_burst(data: &[u8], at: usize, len: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let at = at.min(data.len());
    let mut out = Vec::with_capacity(data.len() + len);
    out.extend_from_slice(&data[..at]);
    // The bytes a wrong baud rate actually produces: framing errors read as
    // dense high-bit noise, not as uniformly random ASCII.
    out.extend((0..len).map(|_| (rng.next_u64() as u8) | 0x80));
    out.extend_from_slice(&data[at..]);
    out
}

/// Cut the stream, as a power loss mid-record does.
pub fn truncate_at(data: &[u8], at: usize) -> Vec<u8> {
    data[..at.min(data.len())].to_vec()
}

/// Drop whole spans, as a device disappearing and reappearing does.
pub fn drop_span(data: &[u8], at: usize, len: usize) -> Vec<u8> {
    let at = at.min(data.len());
    let end = (at + len).min(data.len());
    let mut out = Vec::with_capacity(data.len() - (end - at));
    out.extend_from_slice(&data[..at]);
    out.extend_from_slice(&data[end..]);
    out
}

/// Split a buffer into byte runs of varying size, to drive chunk-independence
/// and reconnect-race tests.
pub fn chunks(data: &[u8], seed: u64, max: usize) -> Vec<Vec<u8>> {
    let mut rng = Rng::new(seed);
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let n = 1 + rng.below(max.max(1));
        let end = (i + n).min(data.len());
        out.push(data[i..end].to_vec());
        i = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injection_is_deterministic_for_a_seed() {
        let data = b"the quick brown fox jumps over the lazy dog".repeat(10);
        assert_eq!(bit_flips(&data, 42, 8), bit_flips(&data, 42, 8));
        assert_ne!(bit_flips(&data, 42, 8), bit_flips(&data, 43, 8));
    }

    #[test]
    fn garbage_burst_is_actually_garbage() {
        let out = garbage_burst(b"clean\n", 3, 512, 7);
        let bad = out.iter().filter(|&&b| b >= 0x80).count();
        assert_eq!(bad, 512);
    }

    #[test]
    fn chunking_preserves_the_bytes() {
        let data = b"abcdefghijklmnopqrstuvwxyz".repeat(20);
        let joined: Vec<u8> = chunks(&data, 9, 7).concat();
        assert_eq!(joined, data);
    }

    #[test]
    fn drop_and_truncate_only_remove() {
        let data = b"0123456789";
        assert_eq!(truncate_at(data, 4), b"0123");
        assert_eq!(drop_span(data, 2, 3), b"0156789");
        assert_eq!(drop_span(data, 8, 100), b"01234567");
    }
}
