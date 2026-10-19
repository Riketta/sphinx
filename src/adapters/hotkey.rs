//! Inbound adapter: the kill hotkey, polled via `GetAsyncKeyState` (no
//! global hook, §3.6). The key combination itself is config-driven
//! (`kill_hotkey = "ctrl+alt+f12"` by default) — nothing is hardcoded.

use tracing::trace;
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

use crate::ports::ControlInput;

/// The ESC virtual-key code, shared by `[abort_key]` validation.
pub const VK_ESCAPE: u8 = 0x1B;

/// The right Windows key; `win` in a hotkey matches either Windows key.
const VK_RWIN: u8 = 0x5C;

/// A parsed hotkey: modifier + main key codes checked on every poll. Each
/// entry is `(primary, alt)` — `alt` is checked as an acceptable substitute
/// (`win` matches either Windows key).
pub struct Hotkey {
    spec: String,
    codes: Vec<(u8, Option<u8>)>,
    /// Whether any part of the combination is a modifier. A modifier-less
    /// kill hotkey would exit sphinx on every tap of the main key.
    has_modifier: bool,
}

impl Hotkey {
    /// Parses a `+`-separated combination like `ctrl+alt+f12` or
    /// `ctrl+shift+q`. Errors name the supported key names so bad config
    /// fails fast at startup instead of disabling the kill switch silently.
    ///
    /// # Errors
    /// Empty input, unknown key names, all-modifier combinations.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut codes = Vec::new();
        let mut all_modifiers = true;
        let mut has_modifier = false;
        for part in spec.split('+') {
            let name = part.trim().to_lowercase();
            if name.is_empty() {
                return Err(format!("empty key part in hotkey {spec:?}"));
            }
            let Some(code) = key_code(&name) else {
                return Err(format!(
                    "unknown key {name:?} in hotkey {spec:?} (supported: ctrl, alt, shift, \
                     win, f1-f24, a-z, 0-9, escape, tab, space, enter, backspace, insert, \
                     delete, home, end, pageup, pagedown, capslock, numlock, scrolllock, menu)"
                ));
            };
            let is_modifier =
                matches!(name.as_str(), "ctrl" | "control" | "alt" | "shift" | "win" | "super");
            all_modifiers &= is_modifier;
            has_modifier |= is_modifier;
            let alt = if name == "win" || name == "super" { Some(VK_RWIN) } else { None };
            codes.push((code, alt));
        }
        if codes.is_empty() || all_modifiers {
            return Err(format!("hotkey {spec:?} needs a non-modifier main key"));
        }
        Ok(Self { spec: spec.to_string(), codes, has_modifier })
    }

    /// The original config string (for `doctor` output).
    pub fn spec(&self) -> &str {
        &self.spec
    }

    /// Whether the combination includes at least one modifier key
    /// (`ctrl`/`alt`/`shift`/`win`).
    pub fn has_modifier(&self) -> bool {
        self.has_modifier
    }

    /// True while every key of the combination is held down.
    pub fn is_down(&self) -> bool {
        self.codes.iter().all(|&(vk, alt)| key_down(vk) || alt.is_some_and(key_down))
    }
}

/// Polled async key state; the documented non-hook way to read a key.
fn key_down(vk: u8) -> bool {
    // SAFETY: polled async key state; the high bit of the i16 result means
    // "currently down".
    unsafe { GetAsyncKeyState(i32::from(vk)) }.is_negative()
}

impl ControlInput for Hotkey {
    fn kill_requested(&mut self) -> bool {
        let down = self.is_down();
        if down {
            trace!("kill hotkey is down");
        }
        down
    }
}

/// Shared key-name -> virtual-key table (kill hotkey + `[abort_key]`).
pub fn key_code(name: &str) -> Option<u8> {
    let named = match name {
        "ctrl" | "control" => 0x11,
        "alt" | "menu" => 0x12,
        "shift" => 0x10,
        "win" | "super" => 0x5B,
        "escape" | "esc" => VK_ESCAPE,
        "tab" => 0x09,
        "space" => 0x20,
        "enter" => 0x0D,
        "backspace" => 0x08,
        "insert" => 0x2D,
        "delete" => 0x2E,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "capslock" => 0x14,
        "numlock" => 0x90,
        "scrolllock" => 0x91,
        _ => 0,
    };
    if named != 0 {
        return Some(named);
    }
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_lowercase() {
            return u8::try_from(u32::from(c) - u32::from('a') + 0x41).ok();
        }
        if c.is_ascii_digit() {
            return u8::try_from(u32::from(c) - u32::from('0') + 0x30).ok();
        }
    }
    name.strip_prefix('f')
        .and_then(|rest| rest.parse::<u32>().ok())
        .filter(|n| (1..=24).contains(n))
        .and_then(|n| u8::try_from(0x70 + n - 1).ok())
}

#[cfg(test)]
mod tests {
    use super::{
        key_code,
        Hotkey,
    };

    #[test]
    fn parses_modifier_combinations() {
        let hk = Hotkey::parse("ctrl+alt+f12").map_err(|e| e.clone());
        assert!(hk.is_ok());
        assert_eq!(hk.map(|h| h.spec().to_string()), Ok("ctrl+alt+f12".to_string()));
    }

    #[test]
    fn rejects_unknown_names() {
        assert!(Hotkey::parse("ctrl+f25").is_err());
        assert!(Hotkey::parse("ctrl+").is_err());
        assert!(Hotkey::parse("").is_err());
    }

    #[test]
    fn rejects_all_modifier_hotkeys() {
        assert!(Hotkey::parse("ctrl+alt").is_err());
        assert!(Hotkey::parse("shift").is_err());
    }

    #[test]
    fn key_table_covers_documented_names() {
        assert_eq!(key_code("escape"), Some(0x1B));
        assert_eq!(key_code("ctrl"), Some(0x11));
        assert_eq!(key_code("a"), Some(0x41));
        assert_eq!(key_code("5"), Some(0x35));
        assert_eq!(key_code("f1"), Some(0x70));
        assert_eq!(key_code("f12"), Some(0x7B));
        assert_eq!(key_code("f24"), Some(0x87));
        assert_eq!(key_code("capslock"), Some(0x14));
        assert_eq!(key_code("numlock"), Some(0x90));
        assert_eq!(key_code("scrolllock"), Some(0x91));
        assert_eq!(key_code("menu"), Some(0x12));
        assert_eq!(key_code("nope"), None);
    }

    #[test]
    fn win_hotkey_parses() -> Result<(), String> {
        let hk = Hotkey::parse("win+q")?;
        assert_eq!(hk.spec(), "win+q");
        Ok(())
    }
}
