//! Inbound adapter: samples the semaphore pixels via `GetPixel` on the
//! screen DC, resolving configured client points against the live game
//! window rect.

use std::time::{
    Duration,
    Instant,
};

use tracing::warn;
use windows::Win32::{
    Graphics::Gdi::{
        GetDC,
        GetPixel,
        ReleaseDC,
        HDC,
    },
    UI::WindowsAndMessaging::IsIconic,
};

use crate::{
    adapters::game_window::{
        window_alive,
        GameWindow,
        GameWindowInfo,
    },
    domain::Rgb,
    ports::SemaphoreReader,
};

/// How often the cached window handle is re-verified against the process
/// list (window *moves* are picked up live by `ClientToScreen`; only a
/// client restart needs a re-find).
const REFRESH_AFTER: Duration = Duration::from_secs(2);

/// How often a re-find may be attempted while the game stays missing (the
/// process snapshot + `EnumWindows` scan is too costly for 20 Hz polling).
const RETRY_FIND_AFTER: Duration = Duration::from_millis(250);

/// How long one identical warning stays suppressed: two configured points
/// failing with *different* messages would otherwise alternate at poll rate
/// (a simple last-message dedup floods in that case).
const WARN_REPEAT: Duration = Duration::from_secs(30);

/// Screen DC held for the reader's lifetime; per-sample `GetDC`/`ReleaseDC`
/// was pure churn at 20 Hz.
struct ScreenDc(HDC);

impl ScreenDc {
    fn acquire() -> Option<Self> {
        // SAFETY: screen DC for the whole desktop; released in `Drop`.
        let hdc = unsafe { GetDC(None) };
        (!hdc.is_invalid()).then_some(Self(hdc))
    }

    fn get(&self) -> HDC {
        self.0
    }
}

impl Drop for ScreenDc {
    fn drop(&mut self) {
        // SAFETY: balancing the `GetDC` in [`ScreenDc::acquire`].
        unsafe {
            let _ = ReleaseDC(None, self.0);
        }
    }
}

/// `SemaphoreReader` over the screen DC. Read failures degrade to
/// [`Rgb::BLACK`] (idle) with rate-limited warnings.
pub struct PixelReader {
    game: GameWindow,
    cmd: (i32, i32),
    strobe: (i32, i32),
    cmd2: Option<(i32, i32)>,
    cached: Option<GameWindowInfo>,
    refreshed_at: Option<Instant>,
    last_find_attempt: Option<Instant>,
    screen_dc: Option<ScreenDc>,
    warn_stamps: Vec<(&'static str, Instant)>,
}

impl PixelReader {
    /// Builds a reader for the given pixel points (client-relative px).
    /// `cmd2` is the pair-extension modifier pixel (None = reads black).
    pub fn new(
        game: GameWindow,
        cmd: (i32, i32),
        strobe: (i32, i32),
        cmd2: Option<(i32, i32)>,
    ) -> Self {
        Self {
            game,
            cmd,
            strobe,
            cmd2,
            cached: None,
            refreshed_at: None,
            last_find_attempt: None,
            screen_dc: None,
            warn_stamps: Vec::new(),
        }
    }

    /// Samples any configured point. Returns exact RGB; `#000000` on failure
    /// (missing/minimized window or a point outside the client rect - reading
    /// foreign pixels must never look like a semaphore color).
    pub fn read_point(&mut self, point: (i32, i32)) -> Rgb {
        let Some(info) = self.window() else {
            self.warn_limited("WoW window not found; pixels read as idle");
            return Rgb::BLACK;
        };
        // SAFETY: read-only iconic-state query on the cached game window.
        if unsafe { IsIconic(info.hwnd) }.as_bool() {
            self.warn_limited("WoW is minimized; pixels read as idle");
            return Rgb::BLACK;
        }
        let (x, y) = point;
        if x < 0 || y < 0 || x >= info.client.0 || y >= info.client.1 {
            self.warn_limited("configured point outside the client rect; pixel read as idle");
            return Rgb::BLACK;
        }
        let Some((sx, sy)) = info.client_to_screen(x, y) else {
            self.warn_limited("ClientToScreen failed (window gone?); pixel read as idle");
            return Rgb::BLACK;
        };
        if self.screen_dc.is_none() {
            self.screen_dc = ScreenDc::acquire();
        }
        let Some(dc) = self.screen_dc.as_ref() else {
            self.warn_limited("GetDC(screen) failed; pixel read as idle");
            return Rgb::BLACK;
        };
        // SAFETY: dc is a live screen DC; coordinates from ClientToScreen.
        let colorref = unsafe { GetPixel(dc.get(), sx, sy) };
        if colorref.0 == u32::MAX {
            // Force a fresh DC next poll: a reset display chain can invalidate
            // the cached one.
            self.screen_dc = None;
            self.warn_limited("GetPixel failed (point outside the screen?); pixel read as idle");
            return Rgb::BLACK;
        }
        let c = colorref.0;
        #[allow(clippy::cast_possible_truncation)] // each channel is masked to 8 bits
        Rgb::new((c & 0xFF) as u8, ((c >> 8) & 0xFF) as u8, ((c >> 16) & 0xFF) as u8)
    }

    fn window(&mut self) -> Option<GameWindowInfo> {
        let fresh = self.cached.as_ref().is_some_and(|info| window_alive(info.hwnd))
            && self.refreshed_at.is_some_and(|at| at.elapsed() < REFRESH_AFTER);
        if fresh {
            return self.cached.clone();
        }
        // The game being closed is the common missing-window case; throttle
        // the expensive re-scan instead of paying it at the poll rate.
        if self.cached.is_none()
            && self.last_find_attempt.is_some_and(|at| at.elapsed() < RETRY_FIND_AFTER)
        {
            return None;
        }
        self.last_find_attempt = Some(Instant::now());
        if let Some(info) = self.game.find() {
            self.refreshed_at = Some(Instant::now());
            self.cached = Some(info.clone());
            Some(info)
        } else {
            self.cached = None;
            None
        }
    }

    /// Decides whether `msg` should warn now: once per message per
    /// [`WARN_REPEAT`], so alternating failures cannot flood the log at
    /// poll rate.
    fn should_warn(&mut self, msg: &'static str) -> bool {
        let now = Instant::now();
        if let Some((_, at)) = self.warn_stamps.iter_mut().find(|(m, _)| *m == msg) {
            if now.duration_since(*at) < WARN_REPEAT {
                return false;
            }
            *at = now;
        } else {
            self.warn_stamps.push((msg, now));
        }
        true
    }

    fn warn_limited(&mut self, msg: &'static str) {
        if self.should_warn(msg) {
            warn!("{msg}");
        }
    }
}

impl SemaphoreReader for PixelReader {
    fn read_cmd(&mut self) -> Rgb {
        self.read_point(self.cmd)
    }

    fn read_cmd2(&mut self) -> Rgb {
        match self.cmd2 {
            Some(point) => self.read_point(point),
            None => Rgb::BLACK,
        }
    }

    fn read_strobe(&mut self) -> Rgb {
        self.read_point(self.strobe)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alternating_failures_do_not_flood() {
        let mut reader = PixelReader::new(GameWindow::new(&[], None), (0, 0), (0, 0), None);
        assert!(reader.should_warn("cmd failed"));
        assert!(reader.should_warn("strobe failed"));
        // The same two messages alternating must be suppressed inside the
        // window: the old last-message-identity dedup emitted at poll rate.
        assert!(!reader.should_warn("cmd failed"));
        assert!(!reader.should_warn("strobe failed"));
    }
}
