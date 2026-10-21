//! Outbound KBM backends. All implement the same [`Kbm`]
//! port: `sendinput` is the default; `serial` (bench-grade hardware input)
//! builds behind the `serial` cargo feature; the `vkbm` RP2040 dongle is
//! the default build; a `window`-message fallback exists only as a
//! rejected plan option.

pub mod guard;
pub mod sendinput;
#[cfg(feature = "serial")]
pub mod serial;
#[cfg(feature = "vkbm")]
pub mod vkbm;

pub use sendinput::SendInputKbm;
#[cfg(feature = "serial")]
pub use serial::SerialKbm;
#[cfg(feature = "vkbm")]
pub use vkbm::VkbmKbm;

/// Whether a screen point lies on the primary monitor - the only surface the
/// OS scales HID absolute reports across (the serial backend's `C` lines and
/// the vkbm absolute fast path both normalize this way).
#[cfg(any(feature = "serial", feature = "vkbm"))]
#[must_use]
pub fn point_in_primary(p: (i32, i32), primary: (i32, i32)) -> bool {
    p.0 >= 0 && p.0 < primary.0 && p.1 >= 0 && p.1 < primary.1
}
