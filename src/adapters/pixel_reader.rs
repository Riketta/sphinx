//! Inbound adapter: samples the semaphore pixels via `GetPixel` on the
//! screen DC, resolving configured client points against the live game
//! window rect (§3.7).

use std::time::{
    Duration,
    Instant,
};

use tracing::warn;
use windows::Win32::Graphics::Gdi::{
    GetDC,
    GetPixel,
    ReleaseDC,
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

/// `SemaphoreReader` over the screen DC. Read failures degrade to
/// [`Rgb::BLACK`] (idle) with rate-limited warnings.
pub struct PixelReader {
    game: GameWindow,
    cmd: (i32, i32),
    strobe: (i32, i32),
    cached: Option<GameWindowInfo>,
    refreshed_at: Option<Instant>,
    last_warn: Option<&'static str>,
}

impl PixelReader {
    /// Builds a reader for the given pixel points (client-relative px).
    pub fn new(game: GameWindow, cmd: (i32, i32), strobe: (i32, i32)) -> Self {
        Self { game, cmd, strobe, cached: None, refreshed_at: None, last_warn: None }
    }

    /// Samples any configured point. Returns exact RGB; `#000000` on failure.
    pub fn read_point(&mut self, point: (i32, i32)) -> Rgb {
        let Some(info) = self.window() else {
            self.warn_limited("WoW window not found; pixels read as idle");
            return Rgb::BLACK;
        };
        let Some((sx, sy)) = info.client_to_screen(point.0, point.1) else {
            self.warn_limited("ClientToScreen failed (window gone?); pixel read as idle");
            return Rgb::BLACK;
        };
        // SAFETY: screen DC for the whole desktop; released below.
        let hdc = unsafe { GetDC(None) };
        if hdc.is_invalid() {
            self.warn_limited("GetDC(screen) failed; pixel read as idle");
            return Rgb::BLACK;
        }
        // SAFETY: hdc from GetDC above; coordinates from ClientToScreen.
        let colorref = unsafe { GetPixel(hdc, sx, sy) };
        // SAFETY: balancing the GetDC call above.
        unsafe {
            let _ = ReleaseDC(None, hdc);
        }
        if colorref.0 == u32::MAX {
            self.warn_limited("GetPixel failed (point outside the screen?); pixel read as idle");
            return Rgb::BLACK;
        }
        self.last_warn = None;
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
        if let Some(info) = self.game.find() {
            self.refreshed_at = Some(Instant::now());
            self.cached = Some(info.clone());
            Some(info)
        } else {
            self.cached = None;
            None
        }
    }

    fn warn_limited(&mut self, msg: &'static str) {
        if self.last_warn != Some(msg) {
            warn!("{msg}");
            self.last_warn = Some(msg);
        }
    }
}

impl SemaphoreReader for PixelReader {
    fn read_cmd(&mut self) -> Rgb {
        self.read_point(self.cmd)
    }

    fn read_strobe(&mut self) -> Rgb {
        self.read_point(self.strobe)
    }
}
