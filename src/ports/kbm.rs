//! Outbound port: actuation against the game UI (§5.2).

/// A key sphinx can send. v1 only needs ESC (ABORT / `BuyDialog` cancel).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// The ESC key.
    Escape,
}

/// Keyboard/mouse backend. Coordinates are **game-window client pixels**;
/// each backend resolves them against the live window rect at use time
/// (§3.7), so window movement never invalidates config.
pub trait Kbm {
    /// Left-clicks at the given client position.
    fn press(&mut self, x: i32, y: i32);

    /// Taps a key (e.g. ESC to dismiss the buy dialog on ABORT).
    fn key(&mut self, key: Key);

    /// Current pointer position in screen coordinates (diagnostics only).
    fn cursor(&self) -> (i32, i32);
}
