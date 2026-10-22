//! Default KBM backend: hardware-level input via enigo (`SendInput`
//! stream). Honors the foreground policy via the shared [`ActuationGuard`]:
//! with `require_foreground`, actuation is refused unless the game window
//! is foreground - sphinx must never click into another window.
//!
//! Coordinate note: the pointer is moved with `SetCursorPos`, which takes
//! physical virtual-screen coordinates - exactly what the per-monitor-aware
//! guard produces. enigo's own `move_mouse(Abs)` is deliberately not used:
//! it normalizes against the *primary* monitor's metrics only (no
//! `MOUSEEVENTF_VIRTUALDESK`), so on a multi-monitor desktop it would land
//! clicks somewhere else.

use std::{
    thread,
    time::Duration,
};

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
        random::OsRandom,
    },
    ports::{
        kbm::{
            Key,
            PressTiming,
        },
        random::jitter_ms,
        Kbm,
    },
};

/// enigo-backed actuator with the foreground guard built in.
pub struct SendInputKbm {
    guard: ActuationGuard,
    enigo: Enigo,
    /// Humanized click timing (hold base + jitter percents).
    timing: PressTiming,
    /// Randomness for the humanized hold.
    rand: OsRandom,
}

/// Moves the cursor to physical virtual-screen coordinates.
fn move_cursor(sx: i32, sy: i32) -> bool {
    // SAFETY: plain cursor move; coordinates come from the guard's
    // ClientToScreen translation.
    unsafe { SetCursorPos(sx, sy) }.is_ok()
}

/// Tolerance for the last-instant cursor re-check (px): the click must land
/// where the policy verified it, not wherever a physical mouse bump left
/// the cursor.
const CLICK_TOLERANCE_PX: i32 = 2;

impl SendInputKbm {
    /// Creates the backend. `timing` shapes the humanized click (hold base
    /// + jitter percents).
    ///
    /// # Errors
    /// If the enigo backend cannot initialize.
    pub fn new(
        require_foreground: bool,
        focus_if_needed: bool,
        game: GameWindow,
        timing: PressTiming,
    ) -> Result<Self, enigo::NewConError> {
        Ok(Self {
            guard: ActuationGuard::new(require_foreground, focus_if_needed, game),
            enigo: Enigo::new(&Settings::default())?,
            timing,
            rand: OsRandom::from_entropy(),
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
        // Press + jittered hold + release instead of enigo's atomic Click:
        // the down->up gap is part of the humanized timing.
        if let Err(e) = self.enigo.button(Button::Left, Direction::Press) {
            warn!(error = %e, "left press failed");
            return false;
        }
        let hold = jitter_ms(self.timing.hold_ms, self.timing.hold_jitter_pct, &mut self.rand);
        thread::sleep(Duration::from_millis(hold));
        if let Err(e) = self.enigo.button(Button::Left, Direction::Release) {
            // The button may be latched; one best-effort retry costs
            // nothing and the release is idempotent.
            let _ = self.enigo.button(Button::Left, Direction::Release);
            warn!(error = %e, "left release failed");
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
            // `Key::Other` resolves to a raw VIRTUAL_KEY on Windows.
            Key::Vk(vk) => enigo::Key::Other(u32::from(vk)),
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
