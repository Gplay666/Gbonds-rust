use std::time::{SystemTime, UNIX_EPOCH};

/// Small deterministic RNG so the player has no extra dependency and can
/// produce reproducible radio queues from a seed.
#[derive(Clone, Debug)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9e37_79b9_7f4a_7c15
            } else {
                seed
            },
        }
    }

    pub fn from_time() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0x1234_5678);
        Self::new(nanos ^ (std::process::id() as u64).rotate_left(17))
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / ((1u64 << 53) as f64)
    }

    pub fn range(&mut self, low: f64, high: f64) -> f64 {
        low + (high - low) * self.next_f64()
    }

    pub fn below(&mut self, upper: usize) -> usize {
        if upper == 0 {
            0
        } else {
            (self.next_u64() % upper as u64) as usize
        }
    }

    pub fn shuffle<T>(&mut self, values: &mut [T]) {
        if values.len() < 2 {
            return;
        }
        for index in (1..values.len()).rev() {
            let other = self.below(index + 1);
            values.swap(index, other);
        }
    }
}
