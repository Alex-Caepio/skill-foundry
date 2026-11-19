//! CPython integer-seeded MT19937 compatibility for deterministic train/holdout splits.
pub const PROFILE: &str = "cpython-mt19937-integer-v1";

pub struct Random {
    state: [u32; 624],
    index: usize,
}

impl Random {
    pub fn new(seed: u64) -> Self {
        let key = if seed > u32::MAX as u64 {
            vec![seed as u32, (seed >> 32) as u32]
        } else {
            vec![seed as u32]
        };
        Self::key(key)
    }

    pub fn key(key: Vec<u32>) -> Self {
        let key = if key.is_empty() { vec![0] } else { key };
        let mut r = Self {
            state: [0; 624],
            index: 624,
        };
        r.state[0] = 19650218;
        for i in 1..624 {
            r.state[i] = 1812433253u32
                .wrapping_mul(r.state[i - 1] ^ (r.state[i - 1] >> 30))
                .wrapping_add(i as u32);
        }
        let (mut i, mut j) = (1, 0);
        for _ in 0..624.max(key.len()) {
            r.state[i] = (r.state[i]
                ^ (r.state[i - 1] ^ (r.state[i - 1] >> 30)).wrapping_mul(1664525))
            .wrapping_add(key[j])
            .wrapping_add(j as u32);
            i += 1;
            j += 1;
            if i >= 624 {
                r.state[0] = r.state[623];
                i = 1;
            }
            if j >= key.len() {
                j = 0;
            }
        }
        for _ in 0..623 {
            r.state[i] = (r.state[i]
                ^ (r.state[i - 1] ^ (r.state[i - 1] >> 30)).wrapping_mul(1566083941))
            .wrapping_sub(i as u32);
            i += 1;
            if i >= 624 {
                r.state[0] = r.state[623];
                i = 1;
            }
        }
        r.state[0] = 0x80000000;
        r
    }

    fn word(&mut self) -> u32 {
        if self.index >= 624 {
            for i in 0..624 {
                let y = (self.state[i] & 0x80000000) | (self.state[(i + 1) % 624] & 0x7fffffff);
                self.state[i] = self.state[(i + 397) % 624] ^ (y >> 1);
                if y & 1 != 0 {
                    self.state[i] ^= 0x9908b0df;
                }
            }
            self.index = 0;
        }
        let mut y = self.state[self.index];
        self.index += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c5680;
        y ^= (y << 15) & 0xefc60000;
        y ^= y >> 18;
        y
    }

    pub fn below(&mut self, n: usize) -> usize {
        if n <= 1 {
            return 0;
        }
        let n = n as u64;
        let bits = 64 - (n - 1).leading_zeros();
        loop {
            let val = if bits <= 32 {
                (self.word() >> (32 - bits)) as u64
            } else {
                let hi = (self.word() as u64) << 32;
                let lo = self.word() as u64;
                (hi | lo) >> (64 - bits)
            };
            if val < n {
                return val as usize;
            }
        }
    }
}
