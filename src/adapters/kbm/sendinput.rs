//! Default KBM backend (§5.4a): hardware-level input via enigo
//! (`SendInput` stream). Honors the foreground policy: with
//! `require_foreground`, actuation is refused unless the game window is
//! foreground — sphinx must never click into another window.

use enigo::{
    Button,
    Coordinate,
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

use crate::{
    adapters::game_window::{
        foreground_hwnd,
        same_window,
        try_focus,
        GameWindow,
        GameWindowInfo,
    },
    ports::{
        kbm::Key,
        Kbm,
    },
};

/// enigo-backed actuator with the §5.4 foreground guard built in.
pub struct SendInputKbm {
    require_foreground: bool,
    focus_if_needed: bool,
    game: GameWindow,
    enigo: Enigo,
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
            require_foreground,
            focus_if_needed,
            game,
            enigo: Enigo::new(&Settings::default())?,
        })
    }

    /// Pointer smoke test for `doctor --smoke`: moves the cursor 1 px and
    /// back. Diagnostics only; no foreground policy involved.
    pub fn smoke_move(&mut self) -> Result<(), enigo::InputError> {
        let (x, y) = self.enigo.location()?;
        self.enigo.move_mouse(x + 1, y + 1, Coordinate::Abs)?;
        info!(from_x = x, from_y = y, "smoke: moved pointer by (+1, +1)");
        self.enigo.move_mouse(x, y, Coordinate::Abs)?;
        info!(to_x = x, to_y = y, "smoke: moved pointer back");
        Ok(())
    }

    /// Finds the game window and enforces the foreground policy. `None`
    /// means "skip this actuation" — the addon's stage timeouts clean up.
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
}

impl Kbm for SendInputKbm {
    fn press(&mut self, x: i32, y: i32) {
        let Some(info) = self.prepared("press") else {
            return;
        };
        let Some((sx, sy)) = info.client_to_screen(x, y) else {
            warn!("ClientToScreen failed; skipping press");
            return;
        };
        if let Err(e) = self.enigo.move_mouse(sx, sy, Coordinate::Abs) {
            warn!(error = %e, "absolute mouse move failed");
            return;
        }
        if let Err(e) = self.enigo.button(Button::Left, Direction::Click) {
            warn!(error = %e, "left click failed");
            return;
        }
        info!(client_x = x, client_y = y, screen_x = sx, screen_y = sy, "press sent");
    }

    fn key(&mut self, key: Key) {
        if self.prepared("key").is_none() {
            return;
        }
        let enigo_key = match key {
            Key::Escape => enigo::Key::Escape,
        };
        if let Err(e) = self.enigo.key(enigo_key, Direction::Click) {
            warn!(error = %e, "key press failed");
            return;
        }
        info!(?key, "key sent");
    }

    fn cursor(&self) -> (i32, i32) {
        self.enigo.location().unwrap_or((0, 0))
    }
}
