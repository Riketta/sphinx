//! Port: a small randomness source for the humanized layer. The domain
//! depends only on this trait, so tests stay deterministic (a zero- or
//! ramp-returning fake); the binary wires in an OS-seeded implementation.

/// Uniform randomness, abstracted for testability.
pub trait Random {
    /// Uniform integer in `[0, n)`; `n == 0` returns 0.
    fn below(&mut self, n: u64) -> u64;
}

/// Offsets `base` by a uniform integer in `[-span, +span]`.
pub fn jitter_i32(base: i32, span: u32, rand: &mut dyn Random) -> i32 {
    if span == 0 {
        return base;
    }
    let offset = i32::try_from(rand.below(u64::from(2 * span + 1))).unwrap_or(0);
    base + offset - span.cast_signed()
}

/// Scales `base` by a uniform factor in `[1 - pct%, 1 + pct%]`.
pub fn jitter_ms(base: u64, pct: u64, rand: &mut dyn Random) -> u64 {
    if base == 0 || pct == 0 {
        return base;
    }
    let spread = base * pct / 100;
    base - spread + rand.below(2 * spread + 1)
}

#[cfg(test)]
mod tests {
    use super::{
        jitter_i32,
        jitter_ms,
        Random,
    };

    /// Cycles through 0, 1, 2, ... - deterministic and exercises the spans.
    struct Ramp(u64);

    impl Random for Ramp {
        fn below(&mut self, n: u64) -> u64 {
            let v = self.0 % n;
            self.0 += 1;
            v
        }
    }

    #[test]
    fn jitter_i32_stays_within_the_span() {
        let mut rand = Ramp(0);
        for _ in 0..50 {
            let v = jitter_i32(100, 3, &mut rand);
            assert!((97..=103).contains(&v), "got {v}");
        }
    }

    #[test]
    fn jitter_i32_with_zero_span_is_exact() {
        let mut rand = Ramp(0);
        assert_eq!(jitter_i32(100, 0, &mut rand), 100);
    }

    #[test]
    fn jitter_ms_stays_within_the_percent_band() {
        let mut rand = Ramp(0);
        for _ in 0..50 {
            let v = jitter_ms(80, 25, &mut rand);
            assert!((60..=100).contains(&v), "got {v}");
        }
    }

    #[test]
    fn jitter_ms_with_zero_base_or_pct_is_exact() {
        let mut rand = Ramp(0);
        assert_eq!(jitter_ms(0, 25, &mut rand), 0);
        assert_eq!(jitter_ms(80, 0, &mut rand), 80);
    }
}
