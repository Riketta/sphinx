//! OS-seeded [`Random`] implementation: splitmix64 seeded from the clock
//! and the process id. Good enough for humanizing actuation; nothing here
//! is cryptographic.

use std::time::{
    SystemTime,
    UNIX_EPOCH,
};

use crate::ports::random::Random;

/// Splitmix64 seeded from OS entropy (clock nanos mixed with the pid).
pub struct OsRandom(u64);

impl OsRandom {
    #[must_use]
    pub fn from_entropy() -> Self {
        #[allow(clippy::cast_possible_truncation)] // 64 bits of nanos is plenty
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0x9E37_79B9_7F4A_7C15, |d| d.as_nanos() as u64);
        let pid = u64::from(std::process::id());
        Self((nanos ^ (pid << 32)) | 1)
    }
}

impl Random for OsRandom {
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        // Modulo bias is irrelevant for the tiny n values used here.
        z % n
    }
}
