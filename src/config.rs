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

/// vkbm backend default USB vendor id (firmware default).
fn d_vkbm_vid() -> u16 {
    0x258a
}

/// vkbm backend default USB product id (firmware default).
fn d_vkbm_pid() -> u16 {
    0x0100
}

/// vkbm backend default per-attempt ack deadline (the vkbm CLI default).
fn d_vkbm_ack_timeout_ms() -> u64 {
    250
}

/// A client-area physical pixel point (§3.7).
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Point {
    /// Horizontal client pixel.
    pub x: i32,
    /// Vertical client pixel.
    pub y: i32,
}

/// Semaphore pixel points, from `/am semaphore coords`. Required: a stale
/// default would sample the wrong pixels silently.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PixelPoints {
    /// CMD pixel.
    pub cmd: Point,
    /// STROBE pixel.
    pub strobe: Point,
}

/// Target button points, from `/am semaphore coords`.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetPoints {
    /// `CommoditiesBuyFrame` `BuyButton`.
    pub buyout: Point,
    /// `BuyDialog` `BuyNowButton`.
    pub confirm: Point,
}

/// ABORT actuation key settings.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
    /// `vkbm`-backend extras (versa-kbm RP2040, §11.4). Everything defaults,
    /// so the table itself is optional.
    #[serde(default)]
    pub vkbm: Option<VkbmBackend>,
}

/// Actuator kind. Also the vocabulary of the `--backend` CLI override.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    /// Hardware-level input via enigo (v1 default).
    #[default]
    Sendinput,
    /// Window-targeted Win32 messages (§5.4b, planned).
    Window,
    /// UART -> HID dongle / MCU (§8, v2).
    Serial,
    /// versa-kbm RP2040 hardware input over the HID vendor channel (§11.4).
    Vkbm,
}

/// Window-backend settings (§5.4b).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowBackend {
    /// Game process image names to discover, case-insensitive.
    #[serde(default = "d_process_names")]
    pub process_names: Vec<String>,
    /// Pin discovery to windows whose title contains this substring
    /// (case-insensitive). Use when several clients are open; without it,
    /// the largest visible client wins.
    #[serde(default)]
    pub title_filter: Option<String>,
}

/// Serial-backend settings (§8).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SerialBackend {
    /// COM port of the dongle/MCU.
    pub port: String,
    /// Line baud rate.
    pub baud: u32,
}

/// How the vkbm backend places the pointer before a click (§11.3).
///
/// - `relative` (default): closed loop - sphinx reads `GetCursorPos`, sends
///   true relative deltas through the board's relative-mouse interface,
///   measures the OS ballistics gain and converges on the target. Reaches
///   every monitor; needs firmware 0.5+.
/// - `abs`: one absolute HID report per click. Fast and deterministic, but
///   Windows normalizes absolute mice to the primary display only.
/// - `auto`: absolute on the primary monitor, closed-loop relative
///   elsewhere.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MoveMode {
    /// Closed-loop relative moves (default; any monitor).
    #[default]
    Relative,
    /// Absolute HID reports (primary monitor only).
    Abs,
    /// Absolute on the primary, relative elsewhere.
    Auto,
}

/// vkbm-backend settings (§11.4): the versa-kbm RP2040 dongle.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VkbmBackend {
    /// USB vendor id to scan for.
    #[serde(default = "d_vkbm_vid")]
    pub vid: u16,
    /// USB product id to scan for.
    #[serde(default = "d_vkbm_pid")]
    pub pid: u16,
    /// Per-attempt ack deadline in ms; a lost ack is resent once (the device
    /// dedups by sequence number, so the resend cannot execute twice).
    #[serde(default = "d_vkbm_ack_timeout_ms")]
    pub ack_timeout_ms: u64,
    /// Pointer placement strategy (see [`MoveMode`]).
    #[serde(default)]
    pub move_mode: MoveMode,
}

impl Default for VkbmBackend {
    fn default() -> Self {
        Self {
            vid: d_vkbm_vid(),
            pid: d_vkbm_pid(),
            ack_timeout_ms: d_vkbm_ack_timeout_ms(),
            move_mode: MoveMode::default(),
        }
    }
}

/// Calibration reference dumped by `/am semaphore coords`; sphinx never
/// derives coordinates from it, but `doctor` compares the live desktop.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Semaphore pixel points (required — no safe default exists).
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

    /// Optional title-substring filter for game-window discovery
    /// (case-insensitive; `None` unless configured non-empty).
    pub fn window_title_filter(&self) -> Option<String> {
        self.backend
            .window
            .as_ref()
            .and_then(|w| w.title_filter.clone())
            .filter(|t| !t.trim().is_empty())
    }

    /// Kill-hotkey hardening beyond [`Hotkey::parse`]: the emergency stop
    /// must not be a trap. A modifier-less combination exits sphinx on every
    /// tap of the main key; `escape` is the ABORT key sphinx itself sends,
    /// so the kill switch would fire right as an abort lands.
    ///
    /// # Errors
    /// Modifier-less hotkey, or any `escape` part in the combination.
    fn validate_kill_hotkey(&self) -> anyhow::Result<()> {
        let kill = Hotkey::parse(&self.kill_hotkey).map_err(|e| {
            anyhow::anyhow!("kill_hotkey {e} (this is your emergency stop - fix it)")
        })?;
        if !kill.has_modifier() {
            bail!(
                "kill_hotkey {:?} has no modifier - it would exit sphinx on every tap of that \
                 key; add ctrl/alt/shift/win",
                self.kill_hotkey
            );
        }
        if self
            .kill_hotkey
            .split('+')
            .any(|part| key_code(&part.trim().to_lowercase()) == Some(VK_ESCAPE))
        {
            bail!(
                "kill_hotkey {:?} must not use escape - it is the ABORT key; pick another key",
                self.kill_hotkey
            );
        }
        Ok(())
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
        if self.debounce_ms >= self.watchdog_ms {
            bail!(
                "debounce_ms {} must be < watchdog_ms {} (else every edge dies in the watchdog \
                 before the debounce window can close and nothing ever actuates)",
                self.debounce_ms,
                self.watchdog_ms
            );
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
        self.validate_kill_hotkey()?;
        // Case-insensitive like the kill hotkey parser: `"Escape"` must not
        // fail where `"escape"` works.
        match key_code(&self.abort_key.key.to_lowercase()) {
            None => bail!("abort_key.key {:?} is not a known key name", self.abort_key.key),
            Some(vk) if vk == VK_ESCAPE => {}
            Some(_) => bail!("abort_key: v1 supports only \"escape\""),
        }
        // An empty list would silently disable window discovery forever.
        if self.process_names().is_empty() {
            bail!(
                "[backend.window] process_names must not be empty - window discovery would \
                   never find anything"
            );
        }
        self.validate_backend_kind()?;
        if let Some(screen) = &self.screen {
            if screen.width <= 0 || screen.height <= 0 {
                bail!("screen width/height must be positive");
            }
        }
        Ok(())
    }

    /// Backend-kind specific validation (§5.4/§8/§11.4): unsupported kinds
    /// and kinds whose feature/table is missing fail here, at startup.
    ///
    /// # Errors
    /// Unsupported or unbuildable backend selections.
    fn validate_backend_kind(&self) -> anyhow::Result<()> {
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
                        "backend.kind = \"serial\" is v2 hardware input; rebuild with --features \
                         serial{detail}"
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
            BackendKind::Vkbm => {
                #[cfg(not(feature = "vkbm"))]
                {
                    let detail = self
                        .backend
                        .vkbm
                        .as_ref()
                        .map(|v| {
                            format!(
                                " (vid {:#06x}, pid {:#06x}, ack_timeout_ms {}, move_mode {:?} \
                                 configured)",
                                v.vid, v.pid, v.ack_timeout_ms, v.move_mode
                            )
                        })
                        .unwrap_or_default();
                    bail!(
                        "backend.kind = \"vkbm\" is versa-kbm hardware input (PLAN §11.4); \
                         rebuild with --features vkbm{detail}"
                    );
                }
                #[cfg(feature = "vkbm")]
                if let Some(v) = self.backend.vkbm.as_ref() {
                    if !(10..=2000).contains(&v.ack_timeout_ms) {
                        bail!("backend.vkbm.ack_timeout_ms {} outside 10..=2000", v.ack_timeout_ms);
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "vkbm")]
    use super::VkbmBackend;
    use super::{
        BackendKind,
        Config,
        ScreenRef,
        WindowBackend,
    };

    /// Full valid config used as the base for `validate` mutation tests.
    const VALID: &str = "[pixels]\ncmd = { x = 7, y = 7 }\nstrobe = { x = 7, y = 15 }\n\
         [targets]\nbuyout = { x = 304, y = 434 }\nconfirm = { x = 285, y = 315 }\n";

    fn parse(toml: &str) -> Result<Config, String> {
        toml::from_str(toml).map_err(|e| e.to_string())
    }

    /// Parses [`VALID`], applies `mutate`, then validates.
    fn validated(mutate: impl FnOnce(&mut Config)) -> Result<(), String> {
        let mut cfg = parse(VALID)?;
        mutate(&mut cfg);
        cfg.validate().map_err(|e| e.to_string())
    }

    #[test]
    fn minimal_config_needs_pixels_and_targets() -> Result<(), String> {
        let cfg = parse(VALID)?;
        assert_eq!(cfg.poll_ms, 50);
        assert_eq!(cfg.kill_hotkey, "ctrl+alt+f12");
        assert!(cfg.require_foreground);
        assert!(!cfg.focus_if_needed);
        assert_eq!(cfg.abort_key.key, "escape");
        assert_eq!(cfg.pixels.cmd, super::Point { x: 7, y: 7 });
        assert!(cfg.screen.is_none());
        Ok(())
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let cfg = parse(
            "nope = 1\n[pixels]\ncmd = { x = 1, y = 2 }\nstrobe = { x = 3, y = 4 }\n\
             [targets]\nbuyout = { x = 1, y = 2 }\nconfirm = { x = 3, y = 4 }\n",
        );
        assert!(cfg.is_err());
    }

    #[test]
    fn missing_pixels_table_is_rejected() {
        // `[pixels]` is required: silent stale defaults would sample the
        // wrong pixels.
        assert!(
            parse("[targets]\nbuyout = { x = 1, y = 2 }\nconfirm = { x = 3, y = 4 }\n").is_err()
        );
    }

    #[test]
    fn unknown_fields_in_nested_tables_are_rejected() {
        let pixels = "[pixels]\ncmd = { x = 1, y = 2 }\nstrobe = { x = 3, y = 4 }\n";
        let targets = "[targets]\nbuyout = { x = 1, y = 2 }\nconfirm = { x = 3, y = 4 }\n";
        assert!(parse(&format!("{pixels}cmdx = {{ x = 1, y = 2 }}\n{targets}")).is_err());
        assert!(parse(&format!("{pixels}{targets}[backend]\nkindd = \"serial\"\n")).is_err());
    }

    #[test]
    fn top_level_reference_keys_parse() -> Result<(), String> {
        // Guards the TOML table trap: `screen`/`ui_scale` are top-level keys
        // and must not be swallowed by whatever [section] precedes them.
        let cfg = parse(
            "screen = { width = 1920, height = 1080 }\nui_scale = 0.65\n[pixels]\ncmd = \
             { x = 1, y = 2 }\nstrobe = { x = 3, y = 4 }\n[targets]\nbuyout = { x = 1, y = 2 }\
             \nconfirm = { x = 3, y = 4 }\n",
        )?;
        assert_eq!(cfg.screen.map(|s| (s.width, s.height)), Some((1920, 1080)));
        assert_eq!(cfg.ui_scale, Some(0.65));
        Ok(())
    }

    #[test]
    fn validate_accepts_the_example_config() -> Result<(), String> {
        parse(VALID)?.validate().map_err(|e| e.to_string())
    }

    #[test]
    fn validate_rejects_debounce_not_below_watchdog() {
        // debounce >= watchdog means every edge dies in the watchdog before
        // the debounce window can close: nothing would ever actuate.
        assert!(validated(|cfg| cfg.debounce_ms = cfg.watchdog_ms).is_err());
    }

    #[test]
    fn validate_rejects_out_of_range_values() {
        assert!(validated(|cfg| cfg.poll_ms = 4).is_err());
        assert!(validated(|cfg| cfg.poll_ms = 1001).is_err());
        assert!(validated(|cfg| cfg.debounce_ms = 0).is_err());
        assert!(validated(|cfg| cfg.watchdog_ms = 99).is_err()); // < 2x poll_ms (100)
        assert!(validated(|cfg| cfg.min_actuation_interval_ms = 60_001).is_err());
        assert!(validated(|cfg| cfg.max_actuations_per_min = 0).is_err());
        assert!(validated(|cfg| cfg.max_actuations_per_min = 601).is_err());
        assert!(validated(|cfg| cfg.screen = Some(ScreenRef { width: 0, height: 1080 })).is_err());
    }

    #[test]
    fn validate_rejects_bad_hotkeys_and_abort_keys() {
        assert!(validated(|cfg| cfg.kill_hotkey = "ctrl+".to_string()).is_err());
        assert!(validated(|cfg| cfg.kill_hotkey = "ctrl+f25".to_string()).is_err());
        // Known but unsupported (v1 pins escape)...
        assert!(validated(|cfg| cfg.abort_key.key = "f5".to_string()).is_err());
        // ...unknown...
        assert!(validated(|cfg| cfg.abort_key.key = "nope".to_string()).is_err());
        // ...and wrongly cased must still work.
        assert!(validated(|cfg| cfg.abort_key.key = "Escape".to_string()).is_ok());
    }

    #[test]
    fn validate_rejects_modifierless_kill_hotkey() {
        // A bare key would exit sphinx on every tap - the default stays valid.
        assert!(validated(|cfg| cfg.kill_hotkey = "q".to_string()).is_err());
        assert!(validated(|cfg| cfg.kill_hotkey = "ctrl+alt+f12".to_string()).is_ok());
    }

    #[test]
    fn validate_rejects_escape_in_kill_hotkey() {
        // Escape is the ABORT key sphinx itself sends; a kill hotkey on it
        // would exit mid-abort. The combination form is rejected too.
        assert!(validated(|cfg| cfg.kill_hotkey = "escape".to_string()).is_err());
        assert!(validated(|cfg| cfg.kill_hotkey = "ctrl+escape".to_string()).is_err());
    }

    #[test]
    fn validate_rejects_empty_process_names() {
        let empty = |cfg: &mut Config| {
            cfg.backend.window =
                Some(WindowBackend { process_names: Vec::new(), title_filter: None });
        };
        assert!(validated(empty).is_err());
    }

    #[cfg(feature = "serial")]
    #[test]
    fn validate_rejects_serial_without_table() {
        assert!(validated(|cfg| cfg.backend.kind = BackendKind::Serial).is_err());
    }

    #[cfg(not(feature = "serial"))]
    #[test]
    fn validate_rejects_serial_kind_without_the_feature() {
        assert!(validated(|cfg| cfg.backend.kind = BackendKind::Serial).is_err());
    }

    #[cfg(feature = "vkbm")]
    #[test]
    fn validate_accepts_vkbm_kind_without_table() {
        // Everything in [backend.vkbm] defaults, so the bare kind is valid.
        assert!(validated(|cfg| cfg.backend.kind = BackendKind::Vkbm).is_ok());
    }

    #[cfg(feature = "vkbm")]
    #[test]
    fn validate_rejects_vkbm_ack_timeout_out_of_range() {
        assert!(validated(|cfg| {
            cfg.backend.kind = BackendKind::Vkbm;
            cfg.backend.vkbm = Some(VkbmBackend { ack_timeout_ms: 9, ..VkbmBackend::default() });
        })
        .is_err());
        assert!(validated(|cfg| {
            cfg.backend.kind = BackendKind::Vkbm;
            cfg.backend.vkbm = Some(VkbmBackend { ack_timeout_ms: 2001, ..VkbmBackend::default() });
        })
        .is_err());
        assert!(validated(|cfg| {
            cfg.backend.kind = BackendKind::Vkbm;
            cfg.backend.vkbm = Some(VkbmBackend::default());
        })
        .is_ok());
    }

    #[cfg(feature = "vkbm")]
    #[test]
    fn vkbm_move_mode_parses_and_defaults_to_relative() -> Result<(), String> {
        // An empty table latches the defaults; the omitted key inside it is
        // the closed-loop relative mode.
        let cfg = parse(&format!("{VALID}[backend]\nkind = \"vkbm\"\n[backend.vkbm]\n"))?
            .backend
            .vkbm
            .ok_or("empty table latches defaults")?;
        assert_eq!(cfg.move_mode, super::MoveMode::Relative);
        let cfg = parse(&format!(
            "{VALID}[backend]\nkind = \"vkbm\"\n[backend.vkbm]\nmove_mode = \"auto\"\n"
        ))?
        .backend
        .vkbm
        .ok_or("table latched")?;
        assert_eq!(cfg.move_mode, super::MoveMode::Auto);
        assert!(parse(&format!(
            "{VALID}[backend]\nkind = \"vkbm\"\n[backend.vkbm]\nmove_mode = \"sideways\"\n"
        ))
        .is_err());
        Ok(())
    }

    #[cfg(not(feature = "vkbm"))]
    #[test]
    fn validate_rejects_vkbm_kind_without_the_feature() {
        assert!(validated(|cfg| cfg.backend.kind = BackendKind::Vkbm).is_err());
    }
}
