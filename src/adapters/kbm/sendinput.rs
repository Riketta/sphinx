//! Default KBM backend (§5.4a): hardware-level input via enigo
//! (`SendInput` stream). Honors the foreground policy via the shared
//! [`ActuationGuard`]: with `require_foreground`, actuation is refused
//! unless the game window is foreground - sphinx must never click into
//! another window.
//!
//! Coordinate note: the pointer is moved with `SetCursorPos`, which takes
//! physical virtual-screen coordinates - exactly what the per-monitor-aware
//! guard produces. enigo's own `move_mouse(Abs)` is deliberately not used:
//! it normalizes against the *primary* monitor's metrics only (no
//! `MOUSEEVENTF_VIRTUALDESK`), so on a multi-monitor desktop it would land
//! clicks somewhere else.

use enigo::{
    Button,
    Direction,
    Enigo,
    Keyboard,
    Mouse,
    Settings,
};
use tracing::{
    info,
    warn,
};
use windows::Win32::UI::WindowsAndMessaging::SetCursorPos;

use crate::{
    adapters::{
        game_window::GameWindow,
        kbm::guard::ActuationGuard,
    },
    ports::{
        kbm::Key,
        Kbm,
    },
};

/// enigo-backed actuator with the foreground guard built in.
pub struct SendInputKbm {
    guard: ActuationGuard,
    enigo: Enigo,
}

/// Moves the cursor to physical virtual-screen coordinates.
fn move_cursor(sx: i32, sy: i32) -> bool {
    // SAFETY: plain cursor move; coordinates come from the guard's
    // ClientToScreen translation.
    unsafe { SetCursorPos(sx, sy) }.is_ok()
}

impl SendInputKbm {
    /// Creates the backend.
    ///
    /// # Errors
    /// If the enigo backend cannot initialize.
    pub fn new(
        require_foreground: bool,
        focus_if_needed: bool,
        game: GameWindow,
    ) -> Result<Self, enigo::NewConError> {
        Ok(Self {
            guard: ActuationGuard::new(require_foreground, focus_if_needed, game),
            enigo: Enigo::new(&Settings::default())?,
        })
    }

    /// Pointer smoke test for `doctor --smoke`: moves the cursor 1 px and
    /// back. Diagnostics only; no foreground policy involved. Fails when the
    /// cursor did not actually move (a cursor pinned at a desktop edge
    /// "succeeds" at `SetCursorPos` without moving).
    pub fn smoke_move(&mut self) -> Result<(), enigo::InputError> {
        let (x, y) = self.enigo.location()?;
        if !move_cursor(x + 1, y + 1) {
            return Err(enigo::InputError::Simulate("SetCursorPos failed"));
        }
        let (mx, my) = self.enigo.location()?;
        if mx == x && my == y {
            let _ = move_cursor(x, y);
            return Err(enigo::InputError::Simulate(
                "cursor did not move (pinned at a screen edge?)",
            ));
        }
        if !move_cursor(x, y) {
            return Err(enigo::InputError::Simulate("SetCursorPos failed"));
        }
        info!(from_x = x, from_y = y, "smoke: moved pointer by (+1, +1) and back");
        Ok(())
    }
}

/// Tolerance for the last-instant cursor re-check (px): the click must land
/// where the policy verified it, not wherever a physical mouse bump left
/// the cursor.
const CLICK_TOLERANCE_PX: i32 = 2;

impl Kbm for SendInputKbm {
    fn press(&mut self, x: i32, y: i32) -> bool {
        let Some((sx, sy)) = self.guard.prepared_point("press", x, y) else {
            return false;
        };
        if !move_cursor(sx, sy) {
            warn!("absolute mouse move failed");
            return false;
        }
        // Re-check the policy with the cursor already in place: a fast
        // alt-tab between discovery and the click must not click another
        // window.
        if !self.guard.still_prepared("press") {
            return false;
        }
        // A physical mouse bump between placement and the click would land
        // the click at the disturbed position: verify the real cursor one
        // last time against the policy-checked point.
        let (cur_x, cur_y) = self.cursor();
        if (cur_x - sx).abs() > CLICK_TOLERANCE_PX || (cur_y - sy).abs() > CLICK_TOLERANCE_PX {
            warn!(
                screen_x = sx,
                screen_y = sy,
                cursor_x = cur_x,
                cursor_y = cur_y,
                "cursor moved after placement; skipping (never click at an unverified position)"
            );
            return false;
        }
        if let Err(e) = self.enigo.button(Button::Left, Direction::Click) {
            warn!(error = %e, "left click failed");
            return false;
        }
        info!(client_x = x, client_y = y, screen_x = sx, screen_y = sy, "press sent");
        true
    }

    fn key(&mut self, key: Key) -> bool {
        if !self.guard.prepared_key("key") {
            return false;
        }
        let enigo_key = match key {
            Key::Escape => enigo::Key::Escape,
        };
        if let Err(e) = self.enigo.key(enigo_key, Direction::Click) {
            warn!(error = %e, "key press failed");
            return false;
        }
        info!(?key, "key sent");
        true
    }

    fn cursor(&self) -> (i32, i32) {
        self.enigo.location().unwrap_or((0, 0))
    }
}
