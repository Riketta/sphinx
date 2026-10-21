//! Outbound port: actuation against the game UI.

/// A key sphinx can send. v1 only needs ESC (ABORT / `BuyDialog` cancel).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// The ESC key.
    Escape,
}

/// Keyboard/mouse backend. Coordinates are **game-window client pixels**;
/// each backend resolves them against the live window rect at use time, so
/// window movement never invalidates config.
pub trait Kbm {
    /// Left-clicks at the given client position. Returns whether the click
    /// was actually delivered (`false` = refused, skipped, or a dry-run
    /// echo — the caller's counters must not claim it).
    fn press(&mut self, x: i32, y: i32) -> bool;

    /// Taps a key (e.g. ESC to dismiss the buy dialog on ABORT). Returns
    /// whether the tap was delivered.
    fn key(&mut self, key: Key) -> bool;

    /// Current pointer position in screen coordinates (diagnostics only).
    fn cursor(&self) -> (i32, i32);
}

/// Click-timing shape for the humanized layer: the down->up hold, and how
/// much the hold and the closed-loop settle gaps are randomized.
#[derive(Clone, Copy, Debug)]
pub struct PressTiming {
    /// Base down->up hold for a click (ms).
    pub hold_ms: u64,
    /// Hold randomized +-this percent around the base (0 = exact).
    pub hold_jitter_pct: u64,
    /// Converge settle gaps randomized +-this percent (0 = exact). Only
    /// the closed-loop (vkbm) backend has settle gaps.
    #[cfg_attr(not(feature = "vkbm"), expect(dead_code))]
    pub settle_jitter_pct: u64,
}
