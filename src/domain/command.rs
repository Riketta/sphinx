//! Domain vocabulary: the exact-RGB color contract (`docs/semaphore.md`)
//! and the commands it decodes to. Pure data; no platform code.

use std::fmt;

/// 8-bit RGB triple sampled from a semaphore pixel. Colors are compared
/// exactly — no blending, no tolerance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
}

impl Rgb {
    /// Idle / no command.
    pub const BLACK: Self = Self { r: 0, g: 0, b: 0 };
    /// Committed CMD on the strobe pixel.
    pub const WHITE: Self = Self { r: 255, g: 255, b: 255 };
    /// BUYOUT.
    pub const RED: Self = Self { r: 255, g: 0, b: 0 };
    /// CONFIRM.
    pub const GREEN: Self = Self { r: 0, g: 255, b: 0 };
    /// ABORT.
    pub const YELLOW: Self = Self { r: 255, g: 255, b: 0 };
    /// AFK.
    pub const MAGENTA: Self = Self { r: 255, g: 0, b: 255 };

    /// Builds an exact RGB triple.
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

impl fmt::Display for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// Action decoded from the CMD pixel color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// `#ff0000`: actuate the native Buyout button.
    Buyout,
    /// `#00ff00`: actuate the Buy dialog's Buy Now button.
    Confirm,
    /// `#ffff00`: press ESC once, stop everything.
    Abort,
    /// `#ff00ff`: tap the configured anti-AFK key once.
    Afk,
}

impl Command {
    /// Decodes a CMD color. Idle (`#000000`) and any non-contract color
    /// (including near-misses) decode to `None`.
    pub fn from_rgb(rgb: Rgb) -> Option<Self> {
        match rgb {
            c if c == Rgb::RED => Some(Self::Buyout),
            c if c == Rgb::GREEN => Some(Self::Confirm),
            c if c == Rgb::YELLOW => Some(Self::Abort),
            c if c == Rgb::MAGENTA => Some(Self::Afk),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Command,
        Rgb,
    };

    #[test]
    fn contract_colors_map_to_commands() {
        assert!(matches!(Command::from_rgb(Rgb::RED), Some(Command::Buyout)));
        assert!(matches!(Command::from_rgb(Rgb::GREEN), Some(Command::Confirm)));
        assert!(matches!(Command::from_rgb(Rgb::YELLOW), Some(Command::Abort)));
        assert!(matches!(Command::from_rgb(Rgb::MAGENTA), Some(Command::Afk)));
    }

    #[test]
    fn idle_and_near_misses_decode_to_none() {
        assert!(Command::from_rgb(Rgb::BLACK).is_none());
        // Near-misses must be rejected: exact colors only.
        assert!(Command::from_rgb(Rgb::new(254, 0, 0)).is_none());
        assert!(Command::from_rgb(Rgb::new(255, 0, 1)).is_none());
        assert!(Command::from_rgb(Rgb::new(1, 2, 3)).is_none());
    }

    #[test]
    fn display_is_hex() {
        assert_eq!(Rgb::new(0x0a, 0xff, 0x00).to_string(), "#0aff00");
    }
}
