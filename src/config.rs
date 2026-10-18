//! Configuration: `sphinx.toml` (serde + TOML). Every operational value —
//! including the kill hotkey — lives here; nothing is hardcoded.

use std::path::Path;

use anyhow::{
    bail,
    Context,
};
use serde::Deserialize;
use tracing::warn;

use crate::adapters::{
    game_window::DEFAULT_PROCESS_NAMES,
    hotkey::{
        key_code,
        Hotkey,
        VK_ESCAPE,
    },
};

/// Poll period default: 50 ms = 20 Hz (contract minimum, §3.4).
fn d_poll_ms() -> u64 {
    50
}

/// Kill hotkey default (§3.6). Change freely in `sphinx.toml`.
fn d_kill_hotkey() -> String {
    "ctrl+alt+f12".to_string()
}

/// Debounce default: two polls >=25 ms apart (§3.3 step 3).
fn d_debounce_ms() -> u64 {
    25
}

/// Strobe watchdog default (§3.6).
fn d_watchdog_ms() -> u64 {
    10_000
}

/// Burst-guard default: the addon's own hand-off cycle is seconds long, so
/// two actuations under 250 ms apart mean a runaway signal source.
fn d_min_interval_ms() -> u64 {
    250
}

/// Burst-guard default: a healthy session stays far below this; the ceiling
/// only exists so a signal storm degrades into skipped actuations (visible,
/// logged) instead of a clicking fit.
fn d_max_per_min() -> u32 {
    30
}

fn d_true() -> bool {
    true
}

fn d_escape() -> String {
    "escape".to_string()
}

fn d_process_names() -> Vec<String> {
    DEFAULT_PROCESS_NAMES.iter().map(|s| (*s).to_string()).collect()
}

/// A client-area physical pixel point (§3.7).
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub struct Point {
    /// Horizontal client pixel.
    pub x: i32,
    /// Vertical client pixel.
    pub y: i32,
}

/// Semaphore pixel points, from `/am semaphore coords`.
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct PixelPoints {
    /// CMD pixel.
    #[serde(default = "d_cmd")]
    pub cmd: Point,
    /// STROBE pixel.
    #[serde(default = "d_strobe")]
    pub strobe: Point,
}

impl Default for PixelPoints {
    fn default() -> Self {
        Self { cmd: d_cmd(), strobe: d_strobe() }
    }
}

fn d_cmd() -> Point {
    Point { x: 8, y: 8 }
}

fn d_strobe() -> Point {
    Point { x: 8, y: 16 }
}

/// Target button points, from `/am semaphore coords`.
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct TargetPoints {
    /// `CommoditiesBuyFrame` `BuyButton`.
    pub buyout: Point,
    /// `BuyDialog` `BuyNowButton`.
    pub confirm: Point,
}

/// ABORT actuation key settings.
#[derive(Clone, Debug, Deserialize)]
pub struct AbortKey {
    /// Key name sphinx sends on ABORT (v1: `"escape"` only).
    #[serde(default = "d_escape")]
    pub key: String,
}

impl Default for AbortKey {
    fn default() -> Self {
        Self { key: d_escape() }
    }
}

/// Backend selection (§5.4/§5.5).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Backend {
    /// Which actuator to use.
    #[serde(default)]
    pub kind: BackendKind,
    /// `window`-backend extras (reserved for §5.4b).
    #[serde(default)]
    pub window: Option<WindowBackend>,
    /// `serial`-backend extras (reserved for §8).
    #[serde(default)]
    pub serial: Option<SerialBackend>,
}

/// Actuator kind.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    /// Hardware-level input via enigo (v1 default).
    #[default]
    Sendinput,
    /// Window-targeted Win32 messages (§5.4b, planned).
    Window,
    /// UART -> HID dongle / MCU (§8, v2).
    Serial,
}

/// Window-backend settings (§5.4b).
#[derive(Clone, Debug, Deserialize)]
pub struct WindowBackend {
    /// Game process image names to discover, case-insensitive.
    #[serde(default = "d_process_names")]
    pub process_names: Vec<String>,
}

/// Serial-backend settings (§8).
#[derive(Clone, Debug, Deserialize)]
pub struct SerialBackend {
    /// COM port of the dongle/MCU.
    pub port: String,
    /// Line baud rate.
    pub baud: u32,
}

/// Calibration reference dumped by `/am semaphore coords`; sphinx never
/// derives coordinates from it, but `doctor` compares the live desktop.
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct ScreenRef {
    /// Physical desktop width.
    pub width: i32,
    /// Physical desktop height.
    pub height: i32,
}

/// Parsed `sphinx.toml`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Poll period in ms (>=20 Hz wanted, so <=50).
    #[serde(default = "d_poll_ms")]
    pub poll_ms: u64,
    /// Kill-switch combination, e.g. `"ctrl+alt+f12"`. HOLD to exit.
    #[serde(default = "d_kill_hotkey")]
    pub kill_hotkey: String,
    /// sendinput refuses to actuate unless `WoW` is foreground.
    #[serde(default = "d_true")]
    pub require_foreground: bool,
    /// sendinput best-effort focuses `WoW` first (Windows may refuse).
    #[serde(default)]
    pub focus_if_needed: bool,
    /// Same-CMD-on-two-polls window (ms).
    #[serde(default = "d_debounce_ms")]
    pub debounce_ms: u64,
    /// Strobe watchdog (ms): white longer than this -> idle, never re-actuate.
    #[serde(default = "d_watchdog_ms")]
    pub watchdog_ms: u64,
    /// Burst guard: minimum gap between two actuations (0 disables).
    #[serde(default = "d_min_interval_ms")]
    pub min_actuation_interval_ms: u64,
    /// Burst guard: rolling per-minute actuation ceiling (>= 1).
    #[serde(default = "d_max_per_min")]
    pub max_actuations_per_min: u32,
    /// Semaphore pixel points.
    #[serde(default)]
    pub pixels: PixelPoints,
    /// Target button points (required — no safe default exists).
    pub targets: TargetPoints,
    /// ABORT key settings.
    #[serde(default)]
    pub abort_key: AbortKey,
    /// Actuator backend.
    #[serde(default)]
    pub backend: Backend,
    /// Live desktop reference from the addon dump.
    #[serde(default)]
    pub screen: Option<ScreenRef>,
    /// Live UI scale reference from the addon dump (informational).
    #[serde(default)]
    pub ui_scale: Option<f64>,
}

impl Config {
    /// Loads and parses `path`.
    ///
    /// # Errors
    /// Missing/unreadable file, TOML syntax/shape errors.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path).with_context(|| {
            format!(
                "reading {} (copy sphinx.toml.example to sphinx.toml and paste \
                 `/am semaphore coords` output)",
                path.display()
            )
        })?;
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    /// Process image names used for game-window discovery.
    pub fn process_names(&self) -> Vec<String> {
        self.backend.window.as_ref().map_or_else(d_process_names, |w| w.process_names.clone())
    }

    /// Cross-field validation; also parses the kill hotkey and abort key so
    /// bad values fail at startup, never mid-session.
    ///
    /// # Errors
    /// Out-of-range timings, unparsable hotkeys, unsupported backend/abort
    /// selections.
    pub fn validate(&self) -> anyhow::Result<()> {
        if !(5..=1000).contains(&self.poll_ms) {
            bail!("poll_ms {} outside 5..=1000", self.poll_ms);
        }
        if self.poll_ms > 50 {
            warn!(
                "poll_ms {} > 50 breaks the <=150 ms actuation budget (contract wants >=20 Hz)",
                self.poll_ms
            );
        }
        if self.debounce_ms == 0 {
            bail!("debounce_ms must be >= 1");
        }
        if self.watchdog_ms < 2 * self.poll_ms {
            bail!("watchdog_ms {} must be >= 2x poll_ms {}", self.watchdog_ms, self.poll_ms);
        }
        if self.min_actuation_interval_ms > 60_000 {
            bail!(
                "min_actuation_interval_ms {} > 60_000 (that disables the sniper, not bursts)",
                self.min_actuation_interval_ms
            );
        }
        if !(1..=600).contains(&self.max_actuations_per_min) {
            bail!("max_actuations_per_min {} outside 1..=600", self.max_actuations_per_min);
        }
        Hotkey::parse(&self.kill_hotkey).map_err(|e| {
            anyhow::anyhow!("kill_hotkey {e} (this is your emergency stop - fix it)")
        })?;
        match key_code(&self.abort_key.key) {
            None => bail!("abort_key.key {:?} is not a known key name", self.abort_key.key),
            Some(vk) if vk == VK_ESCAPE => {}
            Some(_) => bail!("abort_key: v1 supports only \"escape\""),
        }
        match self.backend.kind {
            BackendKind::Sendinput => {}
            BackendKind::Window => {
                bail!("backend.kind = \"window\" is planned (PLAN.md 5.4b); use \"sendinput\"")
            }
            BackendKind::Serial => {
                #[cfg(not(feature = "serial"))]
                {
                    let detail = self
                        .backend
                        .serial
                        .as_ref()
                        .map(|s| format!(" (port {} @ {} baud configured)", s.port, s.baud))
                        .unwrap_or_default();
                    bail!(
                        "backend.kind = \"serial\" is v2 hardware input; rebuild with --features serial{detail}"
                    );
                }
                #[cfg(feature = "serial")]
                let Some(s) = self.backend.serial.as_ref() else {
                    bail!("backend.kind = \"serial\" needs [backend.serial] with port and baud");
                };
                #[cfg(feature = "serial")]
                {
                    if s.port.trim().is_empty() {
                        bail!("backend.serial.port must not be empty");
                    }
                    if s.baud == 0 {
                        bail!("backend.serial.baud must be > 0");
                    }
                }
            }
        }
        if let Some(screen) = &self.screen {
            if screen.width <= 0 || screen.height <= 0 {
                bail!("screen width/height must be positive");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    fn parse(toml: &str) -> Result<Config, String> {
        toml::from_str(toml).map_err(|e| e.to_string())
    }

    #[test]
    fn minimal_config_needs_only_targets() -> Result<(), String> {
        let cfg =
            parse("[targets]\nbuyout  = { x = 304, y = 434 }\nconfirm = { x = 285, y = 315 }\n")?;
        assert_eq!(cfg.poll_ms, 50);
        assert_eq!(cfg.kill_hotkey, "ctrl+alt+f12");
        assert!(cfg.require_foreground);
        assert!(!cfg.focus_if_needed);
        assert_eq!(cfg.abort_key.key, "escape");
        assert!(cfg.screen.is_none());
        Ok(())
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let cfg =
            parse("nope = 1\n[targets]\nbuyout = { x = 1, y = 2 }\nconfirm = { x = 3, y = 4 }\n");
        assert!(cfg.is_err());
    }

    #[test]
    fn top_level_reference_keys_parse() -> Result<(), String> {
        // Guards the TOML table trap: `screen`/`ui_scale` are top-level keys
        // and must not be swallowed by whatever [section] precedes them.
        let cfg = parse(
            "screen = { width = 1920, height = 1080 }\nui_scale = 0.65\n[targets]\nbuyout = \
             { x = 1, y = 2 }\nconfirm = { x = 3, y = 4 }\n",
        )?;
        assert_eq!(cfg.screen.map(|s| (s.width, s.height)), Some((1920, 1080)));
        assert_eq!(cfg.ui_scale, Some(0.65));
        Ok(())
    }
}
