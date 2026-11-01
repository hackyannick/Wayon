//! Bloom-Filter für bereits gesehene URLs: ~1,2 Byte pro URL bei 1 % Fehlerrate
//! statt ~50–100 Byte für ein HashSet<String>.

use xxhash_rust::xxh3::xxh3_128;

pub struct Bloom {
    bits: Vec<u64>,
    nbits: u64,
    k: u32,
}

impl Bloom {
    pub fn new(expected: u64, fp_rate: f64) -> Bloom {
        let expected = expected.max(1000) as f64;
        let ln2 = std::f64::consts::LN_2;
        let nbits = (-(expected * fp_rate.ln()) / (ln2 * ln2)).ceil() as u64;
        let nbits = nbits.max(64);
        let k = ((nbits as f64 / expected) * ln2).round().clamp(1.0, 16.0) as u32;
        Bloom {
            bits: vec![0; nbits.div_ceil(64) as usize],
            nbits,
            k,
        }
    }

    #[inline]
    fn hashes(&self, key: &[u8]) -> (u64, u64) {
        let h = xxh3_128(key);
        (h as u64, ((h >> 64) as u64) | 1)
    }

    pub fn contains(&self, key: &[u8]) -> bool {
        let (h1, h2) = self.hashes(key);
        (0..self.k as u64).all(|i| {
            let b = h1.wrapping_add(i.wrapping_mul(h2)) % self.nbits;
            self.bits[(b / 64) as usize] & (1 << (b % 64)) != 0
        })
    }

    /// Fügt hinzu; liefert `true`, wenn der Schlüssel neu war.
    pub fn insert(&mut self, key: &[u8]) -> bool {
        let (h1, h2) = self.hashes(key);
        let mut new = false;
        for i in 0..self.k as u64 {
            let b = h1.wrapping_add(i.wrapping_mul(h2)) % self.nbits;
            let w = &mut self.bits[(b / 64) as usize];
            let m = 1u64 << (b % 64);
            if *w & m == 0 {
                new = true;
                *w |= m;
            }
        }
        new
    }

    pub fn memory_bytes(&self) -> usize {
        self.bits.len() * 8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic() {
        let mut b = Bloom::new(10_000, 0.01);
        let new = (0..10_000)
            .filter(|i| b.insert(format!("https://x/{i}").as_bytes()))
            .count();
        assert!(new > 9_800, "{new}");
        for i in 0..10_000 {
            assert!(b.contains(format!("https://x/{i}").as_bytes()));
        }
        let fp = (0..10_000)
            .filter(|i| b.contains(format!("https://y/{i}").as_bytes()))
            .count();
        assert!(fp < 300, "zu viele False Positives: {fp}");
    }
}
