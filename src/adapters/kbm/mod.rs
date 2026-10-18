//! Outbound KBM backends. All implement the same [`Kbm`]
//! port (§5.4): `sendinput` is the default; `serial` (hardware input, §8)
//! builds behind the `serial` cargo feature; the `window`-message fallback
//! follows the plan (§5.4b) when needed.

pub mod guard;
pub mod sendinput;
#[cfg(feature = "serial")]
pub mod serial;

pub use sendinput::SendInputKbm;
#[cfg(feature = "serial")]
pub use serial::SerialKbm;
