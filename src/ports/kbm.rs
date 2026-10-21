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
