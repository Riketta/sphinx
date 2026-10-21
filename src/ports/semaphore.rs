//! Inbound port: reading the semaphore pixels (the wire contract:
//! `docs/semaphore.md`).

use crate::domain::Rgb;

/// Reads the two semaphore pixels as exact RGB colors.
pub trait SemaphoreReader {
    /// Samples the CMD pixel.
    ///
    /// Returns [`Rgb::BLACK`] on any read failure (failure degrades to
    /// "idle", which is the safe direction: no actuation).
    fn read_cmd(&mut self) -> Rgb;

    /// Samples the STROBE pixel (same failure semantics).
    fn read_strobe(&mut self) -> Rgb;
}
