//! Outbound KBM backends. All implement the same [`Kbm`]
//! port (§5.4): `sendinput` now; the `window`-message fallback and the
//! `serial` hardware path follow the plan (§5.4b/§8) when needed.

pub mod sendinput;

pub use sendinput::SendInputKbm;
