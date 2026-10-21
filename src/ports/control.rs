//! Inbound port: the kill switch.

/// Human override input: when this reports true, the loop exits immediately.
pub trait ControlInput {
    /// Polls the kill switch (e.g. a hotkey via `GetAsyncKeyState`).
    fn kill_requested(&mut self) -> bool;
}
