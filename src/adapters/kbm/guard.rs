//! Shared pre-actuation guard for KBM backends (§5.4): window discovery plus
//! the foreground policy. `prepared` answers "may this backend actuate now?"
//! and returns the live window info for client→screen coordinate translation
//! (§3.7). A `None` means "skip this actuation" - the addon's own stage
//! timeouts surface it.

use tracing::{
    info,
    warn,
};

use crate::adapters::game_window::{
    foreground_hwnd,
    same_window,
    try_focus,
    GameWindow,
    GameWindowInfo,
};

/// The foreground policy + window discovery, shared by every local backend.
pub struct ActuationGuard {
    require_foreground: bool,
    focus_if_needed: bool,
    game: GameWindow,
}

impl ActuationGuard {
    /// Builds the guard. `require_foreground` refuses actuation unless the
    /// game window is foreground; `focus_if_needed` tries a best-effort
    /// `SetForegroundWindow` first (Windows may refuse).
    pub fn new(require_foreground: bool, focus_if_needed: bool, game: GameWindow) -> Self {
        Self { require_foreground, focus_if_needed, game }
    }

    /// Finds the game window and enforces the foreground policy.
    fn prepared(&mut self, action: &str) -> Option<GameWindowInfo> {
        let Some(info) = self.game.find() else {
            warn!(action, "WoW window not found; skipping");
            return None;
        };
        if !self.require_foreground {
            return Some(info);
        }
        let foreground_ok = foreground_hwnd().is_some_and(|fg| same_window(fg, info.hwnd));
        if foreground_ok {
            return Some(info);
        }
        if self.focus_if_needed {
            info!(action, "WoW lost foreground; trying to focus it");
            if try_focus(info.hwnd) {
                return Some(info);
            }
            warn!(action, "could not focus WoW; skipping");
            return None;
        }
        warn!(action, "WoW is not foreground; skipping (refusing to click into another window)");
        None
    }

    /// Resolves a client point to a screen point behind the policy.
    pub fn prepared_point(&mut self, action: &str, x: i32, y: i32) -> Option<(i32, i32)> {
        let info = self.prepared(action)?;
        let Some((sx, sy)) = info.client_to_screen(x, y) else {
            warn!(action, "ClientToScreen failed; skipping");
            return None;
        };
        Some((sx, sy))
    }

    /// Policy check only (key presses need no coordinate translation).
    pub fn prepared_key(&mut self, action: &str) -> bool {
        self.prepared(action).is_some()
    }
}
