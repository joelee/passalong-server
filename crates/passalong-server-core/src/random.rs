//! Randomness, injected so that upload ids are reproducible in tests.

/// A source of random bytes. The server's own, backed by the operating
/// system, arrives with v0.1.0; the model needs only the seeded one.
pub trait RandomSource: Send {
    /// Fills `bytes`.
    fn fill(&mut self, bytes: &mut [u8]);
}

/// A reproducible source for tests (SplitMix64). Never use it for anything
/// an attacker must not guess.
#[derive(Debug, Clone)]
pub struct SeededRandom(u64);

impl SeededRandom {
    /// A source that repeats itself for the same `seed`.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

impl RandomSource for SeededRandom {
    fn fill(&mut self, bytes: &mut [u8]) {
        for chunk in bytes.chunks_mut(8) {
            let word = self.next().to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seeded_source_repeats_itself_and_differs_by_seed() {
        let mut a = SeededRandom::new(1);
        let mut b = SeededRandom::new(1);
        let mut c = SeededRandom::new(2);
        let (mut x, mut y, mut z) = ([0_u8; 16], [0_u8; 16], [0_u8; 16]);
        a.fill(&mut x);
        b.fill(&mut y);
        c.fill(&mut z);
        assert_eq!(x, y);
        assert_ne!(x, z);
        assert_ne!(x, [0_u8; 16]);
    }
}
