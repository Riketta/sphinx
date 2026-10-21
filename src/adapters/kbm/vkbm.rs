//! `vkbm` KBM backend: the versa-kbm RP2040 dongle, spoken to
//! over its HID vendor control channel (usage page `0xFF00`). Built only
//! with `--features vkbm`; `sphinx.toml` selects it with
//! `backend.kind = "vkbm"`. The domain never sees any of this: same
//! [`Kbm`] port as the enigo backend.
//!
//! Wire contract: fixed 64-byte frames (`seq`, tag, payload, zero padding),
//! one ack per command, one resend on a lost ack. The resend is safe even
//! for actuation because the device dedups by sequence number within a
//! short window - a resent frame is answered from cache, not executed
//! twice. The codec below mirrors `vkbm-proto` (the shared `no_std` crate
//! in the versa-kbm repo; byte-stable since v0.3); the startup handshake
//! pins the protocol major so firmware/codec drift fails loudly instead of
//! clicking wrong.
//!
//! Coordinates are screen pixels translated by the shared [`ActuationGuard`]
//! like every other backend. Where the pointer goes from there is a
//! `move_mode` choice ([`MoveMode`]): `relative` (the default) converges on
//! the target with a closed loop over true relative reports - probe, measure
//! the OS ballistics gain, correct - which reaches every monitor; `abs`
//! sends one absolute HID report (`0..=32767`), which Windows normalizes
//! across the primary display only; `auto` picks abs on the primary and the
//! closed loop elsewhere.

use std::{
    fmt,
    path::Path,
    thread,
    time::{
        Duration,
        Instant,
        SystemTime,
        UNIX_EPOCH,
    },
};

use anyhow::{
    anyhow,
    bail,
    Context,
};
use hidapi::{
    HidApi,
    HidDevice,
};
use serde::{
    Deserialize,
    Serialize,
};
use tracing::{
    debug,
    info,
    warn,
};
use windows::Win32::{
    Foundation::POINT,
    UI::WindowsAndMessaging::{
        GetCursorPos,
        GetSystemMetrics,
        SM_CXVIRTUALSCREEN,
        SM_CYVIRTUALSCREEN,
        SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    },
};

use crate::{
    adapters::{
        game_window::{
            screen_size,
            GameWindow,
        },
        kbm::{
            guard::ActuationGuard,
            point_in_primary,
        },
        random::OsRandom,
    },
    config::MoveMode,
    ports::{
        kbm::{
            Key,
            PressTiming,
        },
        random::jitter_ms,
        Kbm,
    },
};

/// Vendor control channel usage page, from the firmware report descriptor.
const VENDOR_USAGE_PAGE: u16 = 0xff00;

/// Vendor control channel usage, from the firmware report descriptor.
const VENDOR_USAGE: u16 = 0x01;

/// HID keyboard usage for ESC (what [`Key::Escape`] maps to on the wire).
const USAGE_ESCAPE: u8 = 0x29;

/// Protocol major this backend speaks; `new` refuses anything else.
const PROTOCOL_MAJOR: u8 = 0;

/// Firmware minor that introduced the true relative pointer (fw 0.5).
/// Before it, `MoveRel` re-emitted an absolute report at the device's
/// tracked anchor - position-unknown under ballistics and user interference,
/// so a closed loop cannot trust it. Modes that move relatively require at
/// least this minor at init.
const RELATIVE_FW_MINOR: u8 = 5;

/// Highest coordinate of the HID absolute-mouse mapping (logical maximum).
const ABS_MAX: u16 = 32_767;

/// Clicks land within this distance of the target (screen px per axis).
/// Two, not one: a 1-count report can legally move the cursor 0 px (the
/// OS rounds), so demanding 1 px bounces the last mile against
/// quantization - and the button is tens of pixels wide anyway.
const CONVERGE_TOLERANCE_PX: i32 = 2;
/// Probe + corrections allowed per closed-loop move. The probe is the
/// whole-distance fast path; under ballistics the walk-back costs a
/// handful of adapted corrections, each a few milliseconds round-trip. A
/// broken move is caught by `Pinned` (probe ignored), the deadline and
/// the caller's click refusal - not by starving this budget.
const CONVERGE_MAX_NUDGES: u32 = 16;
/// Whole-move budget. Chunked streams legitimately need tens of ms per
/// nudge (the device paces the <=127-count reports 4 ms apart and the ack
/// waits for the last one), so the original 150 ms flat cap stalled
/// distant moves right after the probe. A pathological loop still ends
/// here, and any non-convergence refuses the click.
const CONVERGE_DEADLINE: Duration = Duration::from_millis(2000);
/// Ballistics gain estimates are clamped to this band so one nonsense
/// measurement cannot blow the next correction up; corrections are
/// additionally capped at twice the remaining error (see `step_axis`).
const GAIN_MIN: f64 = 1.0 / 64.0;
const GAIN_MAX: f64 = 64.0;

/// Overshoot allowance on top of the twice-the-error correction cap.
const CORRECTION_HEADROOM_PX: i32 = 16;

/// How much a per-axis step shrinks when it overshoots the target: the
/// measured gain mispredicted the next report's ballistics bucket, and
/// repeated mispredictions must contract instead of ping-ponging.
const OVERSHOOT_SHRINK: f64 = 0.5;
/// How much a damped step regrows after landing short again. Bounded at
/// 1.0 and slower than the shrink - re-entering an overshoot costs a
/// flip, landing short again costs nothing.
const STEP_REGROW: f64 = 1.5;
/// Floor for the damped step scale: below this the one-count signum
/// fixup is the only progress anyway.
const STEP_SCALE_MIN: f64 = 0.125;

/// Deliberate under-correction of every gain-scaled step: a mispredicted
/// ballistics bucket then lands short of the target (one more nudge)
/// instead of crossing it (a flip that must be damped back). This is
/// what turns the live-observed back-and-forth bounce into a mostly
/// monotone approach.
const CORRECTION_FACTOR: f64 = 0.9;

/// Recent (step size, measured gain) samples kept per axis. The live
/// traces show the gain is a stable function of report size within a
/// movement (3 counts ~ 0.5x, 12-27 counts ~ 1.5x), so the gain measured
/// at the most similar size predicts the next step far better than the
/// last step's gain does. Recent only: right after an idle the whole
/// curve sits lower (the OS speed estimate ramps up) - old samples from
/// a dead regime would mispredict.
const PROFILE_SAMPLES: usize = 8;

/// Finds the vendor control channel interface among HID devices.
#[must_use]
fn find_device(api: &HidApi, vid: u16, pid: u16) -> Option<hidapi::DeviceInfo> {
    api.device_list()
        .find(|d| {
            d.vendor_id() == vid
                && d.product_id() == pid
                && d.usage_page() == VENDOR_USAGE_PAGE
                && d.usage() == VENDOR_USAGE
        })
        .cloned()
}

/// Wire codec mirroring `vkbm-proto`: the command subset sphinx sends, the
/// full reply vocabulary, and the 64-byte framing. Keep in sync with the
/// shared crate; the protocol-major handshake at init catches drift.
mod wire {
    use core::fmt;

    /// Report size of the vendor control channel.
    pub(super) const FRAME_LEN: usize = 64;

    // Command tags (host -> device), the subset sphinx sends. Values are
    // fixed by the wire contract; do not renumber.
    const TAG_PING: u8 = 0x01;
    const TAG_INFO: u8 = 0x02;
    const TAG_KEY_TAP: u8 = 0x12;
    const TAG_MOVE_ABS: u8 = 0x20;
    const TAG_MOVE_REL: u8 = 0x21;
    const TAG_MOUSE_DOWN: u8 = 0x22;
    const TAG_MOUSE_UP: u8 = 0x23;
    const TAG_CLICK_ABS: u8 = 0x24;
    const TAG_RELEASE_ALL: u8 = 0x30;
    const TAG_DRY_RUN: u8 = 0x34;

    // Reply tags (device -> host).
    const TAG_REPLY_OK: u8 = 0x00;
    const TAG_REPLY_ECHO: u8 = 0x01;
    const TAG_REPLY_INFO: u8 = 0x02;
    const TAG_REPLY_ERR: u8 = 0xFF;

    // Error codes inside [`Reply::Err`].
    const TAG_ERR_UNKNOWN_COMMAND: u8 = 0x01;
    const TAG_ERR_BAD_PAYLOAD: u8 = 0x02;
    const TAG_ERR_DISARMED: u8 = 0x03;
    const TAG_ERR_NOT_IMPLEMENTED: u8 = 0x04;
    const TAG_ERR_KEYBOARD_FULL: u8 = 0x05;
    const TAG_ERR_DISABLED: u8 = 0x06;

    /// Wire tag of the primary (left) mouse button.
    pub(super) const BUTTON_LEFT: u8 = 1;

    /// Bits of the [`Reply::Info`] `flags` byte (device gate states).
    /// `pub` is contained by the private `mod wire`.
    pub(super) mod info_flags {
        /// The hardware switch on the board is on.
        pub const SWITCH_ON: u8 = 0x01;

        /// The soft arm gate is open.
        pub const ARMED: u8 = 0x02;

        /// Dry-run mode is on (actuation echoed, not executed).
        pub const DRY_RUN: u8 = 0x04;
    }

    /// One host-to-device command (the subset sphinx uses).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Cmd {
        /// Liveness probe.
        Ping,
        /// Firmware version query.
        Info,
        /// Presses and releases a keyboard usage.
        KeyTap {
            /// HID keyboard usage code.
            usage: u8,
        },
        /// Moves the pointer to an absolute position. No production sender
        /// today - the press path uses [`Cmd::ClickAbs`] and the smoke probe
        /// was moved to a zero-delta [`Cmd::MoveRel`] - but the variant stays
        /// for mirror completeness with `vkbm-proto` (and codec tests).
        #[expect(dead_code)]
        MoveAbs {
            /// Absolute X, `0..=32767`.
            x: u16,
            /// Absolute Y, `0..=32767`.
            y: u16,
        },
        /// Moves the pointer by a signed delta (a true relative report; the
        /// OS applies its pointer ballistics to it).
        MoveRel {
            /// Signed X delta.
            dx: i16,
            /// Signed Y delta.
            dy: i16,
        },
        /// Presses the primary (left) button down. The device routes button
        /// reports to the pointer interface that moved last.
        MouseDown,
        /// Releases the primary button.
        MouseUp,
        /// Moves to an absolute position, then clicks the primary button.
        ClickAbs {
            /// Absolute X, `0..=32767`.
            x: u16,
            /// Absolute Y, `0..=32767`.
            y: u16,
        },
        /// Releases every held key and button (release-class on the device:
        /// it bypasses the gates and dedup-replays like any other command).
        ReleaseAll,
        /// Toggles device dry-run (echoes instead of actuating).
        DryRun {
            /// `true` turns dry-run on.
            enabled: bool,
        },
    }

    impl fmt::Display for Cmd {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match *self {
                Self::Ping => f.write_str("ping"),
                Self::Info => f.write_str("info"),
                Self::KeyTap { usage } => write!(f, "key-tap usage={usage}"),
                Self::MoveAbs { x, y } => write!(f, "move-abs x={x} y={y}"),
                Self::MoveRel { dx, dy } => write!(f, "move-rel dx={dx} dy={dy}"),
                Self::MouseDown => f.write_str("mouse-down button=left"),
                Self::MouseUp => f.write_str("mouse-up button=left"),
                Self::ClickAbs { x, y } => write!(f, "click-abs x={x} y={y}"),
                Self::ReleaseAll => f.write_str("release-all"),
                Self::DryRun { enabled } => write!(f, "dry-run enabled={enabled}"),
            }
        }
    }

    /// Device-to-host reply.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Reply {
        /// Command accepted and executed.
        Ok,
        /// Actuation accepted but not executed: the device is in dry-run.
        Echo,
        /// Answer to [`Cmd::Info`].
        Info {
            /// Semver major.
            major: u8,
            /// Semver minor.
            minor: u8,
            /// [`info_flags`] bits.
            flags: u8,
        },
        /// Command rejected.
        Err(ErrorCode),
    }

    /// Rejection reasons (device-side).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum ErrorCode {
        /// Unknown command tag.
        UnknownCommand,
        /// Payload had the wrong shape.
        BadPayload,
        /// Device is disarmed; the host can re-arm with a config tool.
        Disarmed,
        /// Recognized but not wired into this firmware build.
        NotImplemented,
        /// Keyboard 6KRO budget already full.
        KeyboardFull,
        /// The hardware switch on the board is off.
        Disabled,
    }

    impl fmt::Display for ErrorCode {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::UnknownCommand => f.write_str("unknown command"),
                Self::BadPayload => f.write_str("bad payload"),
                Self::Disarmed => f.write_str("disarmed (run `vkbm arm`)"),
                Self::NotImplemented => f.write_str("not implemented in this firmware"),
                Self::KeyboardFull => f.write_str("keyboard 6KRO budget full"),
                Self::Disabled => f.write_str("hardware switch is off (tap BOOTSEL on the board)"),
            }
        }
    }

    /// Writes one byte at a fixed offset without slice indexing.
    fn put_byte(frame: &mut [u8; FRAME_LEN], offset: usize, v: u8) {
        if let Some(slot) = frame.get_mut(offset) {
            *slot = v;
        }
    }

    /// Writes a little-endian `u16` at a fixed offset without slice indexing.
    fn put_le16(frame: &mut [u8; FRAME_LEN], offset: usize, v: u16) {
        let bytes = v.to_le_bytes();
        let (_, tail) = frame.split_at_mut(offset);
        let (slot, _) = tail.split_at_mut(bytes.len());
        slot.copy_from_slice(&bytes);
    }

    /// Encodes `cmd` into `out` under sequence number `seq`, zero-padding
    /// the remainder (mirror of `vkbm_proto::frame::encode`).
    pub(super) fn encode(cmd: Cmd, seq: u8, out: &mut [u8; FRAME_LEN]) {
        *out = [0u8; FRAME_LEN];
        out[0] = seq;
        match cmd {
            Cmd::Ping => put_byte(out, 1, TAG_PING),
            Cmd::Info => put_byte(out, 1, TAG_INFO),
            Cmd::KeyTap { usage } => {
                put_byte(out, 1, TAG_KEY_TAP);
                put_byte(out, 2, usage);
            }
            Cmd::MoveAbs { x, y } => {
                put_byte(out, 1, TAG_MOVE_ABS);
                put_le16(out, 2, x);
                put_le16(out, 4, y);
            }
            Cmd::MoveRel { dx, dy } => {
                put_byte(out, 1, TAG_MOVE_REL);
                put_le16(out, 2, u16::from_le_bytes(dx.to_le_bytes()));
                put_le16(out, 4, u16::from_le_bytes(dy.to_le_bytes()));
            }
            Cmd::MouseDown => {
                put_byte(out, 1, TAG_MOUSE_DOWN);
                put_byte(out, 2, BUTTON_LEFT);
            }
            Cmd::MouseUp => {
                put_byte(out, 1, TAG_MOUSE_UP);
                put_byte(out, 2, BUTTON_LEFT);
            }
            Cmd::ClickAbs { x, y } => {
                put_byte(out, 1, TAG_CLICK_ABS);
                put_le16(out, 2, x);
                put_le16(out, 4, y);
            }
            Cmd::ReleaseAll => put_byte(out, 1, TAG_RELEASE_ALL),
            Cmd::DryRun { enabled } => {
                put_byte(out, 1, TAG_DRY_RUN);
                put_byte(out, 2, u8::from(enabled));
            }
        }
    }

    /// Payload helpers accept trailing zero padding: the wire always carries
    /// full 64-byte reports, so a short payload arrives padded with zeros;
    /// nonzero bytes beyond the expected payload are rejected as junk.
    fn empty(payload: &[u8]) -> Result<(), ()> {
        if payload.iter().all(|&b| b == 0) {
            Ok(())
        } else {
            Err(())
        }
    }

    fn one(payload: &[u8]) -> Result<u8, ()> {
        match payload {
            [b] => Ok(*b),
            [b, rest @ ..] if rest.iter().all(|&x| x == 0) => Ok(*b),
            _ => Err(()),
        }
    }

    fn three(payload: &[u8]) -> Result<[u8; 3], ()> {
        match payload {
            [a, b, c] => Ok([*a, *b, *c]),
            [a, b, c, rest @ ..] if rest.iter().all(|&x| x == 0) => Ok([*a, *b, *c]),
            _ => Err(()),
        }
    }

    /// Decodes one reply frame; `Err(())` for junk (too short, unknown tag,
    /// payload mismatch) - the ack policy merely counts it.
    pub(super) fn decode_reply(data: &[u8]) -> Result<(u8, Reply), ()> {
        let seq = *data.first().ok_or(())?;
        let kind = *data.get(1).ok_or(())?;
        let payload = data.get(2..).ok_or(())?;
        let reply = match kind {
            TAG_REPLY_OK => {
                empty(payload)?;
                Reply::Ok
            }
            TAG_REPLY_ECHO => {
                empty(payload)?;
                Reply::Echo
            }
            TAG_REPLY_INFO => {
                let [major, minor, flags] = three(payload)?;
                Reply::Info { major, minor, flags }
            }
            TAG_REPLY_ERR => Reply::Err(match one(payload)? {
                TAG_ERR_UNKNOWN_COMMAND => ErrorCode::UnknownCommand,
                TAG_ERR_BAD_PAYLOAD => ErrorCode::BadPayload,
                TAG_ERR_DISARMED => ErrorCode::Disarmed,
                TAG_ERR_NOT_IMPLEMENTED => ErrorCode::NotImplemented,
                TAG_ERR_KEYBOARD_FULL => ErrorCode::KeyboardFull,
                TAG_ERR_DISABLED => ErrorCode::Disabled,
                _ => return Err(()),
            }),
            _ => return Err(()),
        };
        Ok((seq, reply))
    }

    /// Encodes a reply under `seq` - test-only, so policy tests can stage
    /// device responses without hand-building frames.
    #[cfg(test)]
    pub(super) fn encode_reply(seq: u8, reply: Reply, out: &mut [u8; FRAME_LEN]) {
        *out = [0u8; FRAME_LEN];
        out[0] = seq;
        match reply {
            Reply::Ok => put_byte(out, 1, TAG_REPLY_OK),
            Reply::Echo => put_byte(out, 1, TAG_REPLY_ECHO),
            Reply::Info { major, minor, flags } => {
                put_byte(out, 1, TAG_REPLY_INFO);
                put_byte(out, 2, major);
                put_byte(out, 3, minor);
                put_byte(out, 4, flags);
            }
            Reply::Err(code) => {
                put_byte(out, 1, TAG_REPLY_ERR);
                put_byte(
                    out,
                    2,
                    match code {
                        ErrorCode::UnknownCommand => TAG_ERR_UNKNOWN_COMMAND,
                        ErrorCode::BadPayload => TAG_ERR_BAD_PAYLOAD,
                        ErrorCode::Disarmed => TAG_ERR_DISARMED,
                        ErrorCode::NotImplemented => TAG_ERR_NOT_IMPLEMENTED,
                        ErrorCode::KeyboardFull => TAG_ERR_KEYBOARD_FULL,
                        ErrorCode::Disabled => TAG_ERR_DISABLED,
                    },
                );
            }
        }
    }
}

/// Transport failures, split so the ack policy retries only genuine losses.
#[derive(Debug)]
enum LinkError {
    /// The OS refused the write/read.
    Io(String),
    /// Nothing arrived within the deadline.
    Timeout,
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(msg) => f.write_str(msg),
            Self::Timeout => f.write_str("timed out"),
        }
    }
}

/// One host-to-device report write plus one device-to-host report read.
/// Abstracted so the ack/retry policy is testable without hardware.
trait Link {
    /// Writes one 64-byte frame.
    fn send(&mut self, report: &[u8; 64]) -> Result<(), LinkError>;

    /// Reads one 64-byte report, or [`LinkError::Timeout`].
    fn recv(&mut self, timeout: Duration) -> Result<[u8; 64], LinkError>;
}

/// Real link over a `hidapi` device handle.
struct HidLink {
    device: HidDevice,
}

impl Link for HidLink {
    fn send(&mut self, report: &[u8; 64]) -> Result<(), LinkError> {
        // Report-id prefix: the device uses unnumbered reports, so the id
        // byte is 0.
        let mut out = [0u8; 65];
        let (_, dst) = out.split_at_mut(1);
        dst.copy_from_slice(report);
        let wrote = self.device.write(&out).map_err(|e| LinkError::Io(e.to_string()))?;
        if wrote != out.len() {
            return Err(LinkError::Io(format!("short write: {wrote} of {}", out.len())));
        }
        Ok(())
    }

    fn recv(&mut self, timeout: Duration) -> Result<[u8; 64], LinkError> {
        // Zeroed first: a short read must not mix stale bytes into the frame.
        let mut buf = [0u8; 64];
        let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        match self.device.read_timeout(&mut buf, millis) {
            Ok(n) if n > 0 => Ok(buf),
            Ok(_) => Err(LinkError::Timeout),
            Err(e) => Err(LinkError::Io(e.to_string())),
        }
    }
}

/// Seeds the per-process sequence counter: the device dedups by sequence
/// number, so a fresh sphinx must not start where the previous session left
/// off - the device would replay that session's cached reply and swallow
/// the command. Clock subsecond nanos mixed with the process id spread the
/// starting points; 0 is the device's never-executed slot, so it maps to 1.
fn initial_seq() -> u8 {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let pid = std::process::id();
    #[allow(clippy::cast_possible_truncation)] // a byte-wide mix is the point
    {
        let mixed = (nanos as u8)
            ^ ((nanos >> 8) as u8)
            ^ (pid as u8)
            ^ ((pid >> 8) as u8)
            ^ ((pid >> 16) as u8);
        if mixed == 0 {
            1
        } else {
            mixed
        }
    }
}

/// Why a request did not complete.
#[derive(Debug)]
enum RequestError {
    /// No ack for the send plus its one resend: the device may or may not
    /// have executed (the dedup keeps a resent frame from executing twice).
    Lost,
    /// The transport broke; retrying cannot help.
    Link(LinkError),
}

/// Sends one command frame with the ack policy: a lost ack gets exactly one
/// resend before the request is given up on (mirror of the vkbm CLI). Safe
/// for actuation because of the device's sequence dedup.
fn issue(
    link: &mut dyn Link,
    raw: &[u8; 64],
    seq: u8,
    timeout: Duration,
) -> Result<wire::Reply, RequestError> {
    for attempt in 0..2 {
        link.send(raw).map_err(RequestError::Link)?;
        match wait_ack(link, seq, timeout) {
            Ok(reply) => return Ok(reply),
            Err(RequestError::Lost) if attempt == 0 => {}
            Err(e) => return Err(e),
        }
    }
    Err(RequestError::Lost)
}

/// Waits for the ack of one attempt. One deadline spans the whole wait:
/// stale replies (for an earlier, timed-out request) must not extend it,
/// and a stream of stale or undecodable frames is capped instead of looping
/// forever.
fn wait_ack(link: &mut dyn Link, seq: u8, timeout: Duration) -> Result<wire::Reply, RequestError> {
    /// How many junk frames one attempt tolerates before declaring it lost.
    const MAX_JUNK: u32 = 8;
    let deadline = Instant::now() + timeout;
    let mut junk = 0u32;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(RequestError::Lost);
        }
        // A read that expires with nothing to show is a lost attempt (the
        // retry policy decides what that is worth); only genuine transport
        // failures are fatal for the request.
        let raw = match link.recv(deadline - now) {
            Ok(raw) => raw,
            Err(LinkError::Timeout) => return Err(RequestError::Lost),
            Err(e) => return Err(RequestError::Link(e)),
        };
        match wire::decode_reply(&raw) {
            Ok((got, reply)) if got == seq => return Ok(reply),
            _ => junk += 1,
        }
        if junk >= MAX_JUNK {
            return Err(RequestError::Lost);
        }
    }
}

/// Request client over the vendor control channel: sequence numbering plus
/// the ack policy around a [`Link`].
struct Client {
    link: Box<dyn Link>,
    next_seq: u8,
}

impl Client {
    /// Wraps an open device handle.
    fn new(device: HidDevice) -> Self {
        Self { link: Box::new(HidLink { device }), next_seq: initial_seq() }
    }

    /// Sends one command and waits for its ack; a lost ack is resent once.
    fn request(&mut self, cmd: wire::Cmd, timeout: Duration) -> Result<wire::Reply, RequestError> {
        let seq = self.next_seq;
        self.next_seq = if seq == u8::MAX { 1 } else { seq + 1 };
        let mut raw = [0u8; 64];
        wire::encode(cmd, seq, &mut raw);
        issue(self.link.as_mut(), &raw, seq, timeout)
    }
}

/// Maps a handshake-request failure to an error: a lost ack is a different
/// story (device silent) than a broken transport.
fn request_error(err: RequestError) -> anyhow::Error {
    match err {
        RequestError::Lost => anyhow::anyhow!("no ack (send + one resend)"),
        RequestError::Link(e) => anyhow::anyhow!("transport failed: {e}"),
    }
}

/// Maps a screen pixel to the HID absolute range, endpoint-exact: 0 -> 0
/// and `dim - 1` -> [`ABS_MAX`], matching how the OS scales the range back
/// across the primary monitor.
#[must_use]
fn to_abs(v: i32, dim: i32) -> u16 {
    if dim <= 1 {
        return 0;
    }
    let clamped = v.clamp(0, dim - 1);
    let scaled = (i64::from(clamped) * i64::from(ABS_MAX)) / i64::from(dim - 1);
    u16::try_from(scaled).unwrap_or(ABS_MAX)
}

/// Interprets an actuation outcome: `Ok` and dry-run `Echo` are both
/// success; a lost ack is logged loudly but never retried (the dedup keeps
/// a resent frame from executing twice, so a retry could only double-send).
fn report_actuation(action: &str, cmd: wire::Cmd, outcome: Result<wire::Reply, RequestError>) {
    match outcome {
        Ok(wire::Reply::Ok) => info!(action, command = %cmd, "vkbm command acked"),
        Ok(wire::Reply::Echo) => {
            info!(action, command = %cmd, "vkbm dry-run echo (not actuated)");
        }
        Ok(wire::Reply::Err(code)) => warn!(action, command = %cmd, "vkbm refused: {code}"),
        Ok(other) => warn!(action, command = %cmd, "vkbm unexpected reply {other:?}"),
        Err(RequestError::Lost) => warn!(
            action,
            command = %cmd,
            "vkbm ack lost (send + resend) - outcome unknown; skipping (the addon's stage \
             timeouts surface the gap)"
        ),
        Err(RequestError::Link(e)) => {
            warn!(action, command = %cmd, "vkbm transport failed: {e}");
        }
    }
}

/// Whether a movement mode needs the firmware's true relative pointer at
/// all (fw 0.5+).
fn relative_requested(mode: MoveMode) -> bool {
    matches!(mode, MoveMode::Relative | MoveMode::Auto)
}

/// Whether this move should take the closed-loop path: `Relative` always,
/// `Abs` never, `Auto` everywhere but the primary monitor.
fn wants_relative(mode: MoveMode, point: (i32, i32), primary: (i32, i32)) -> bool {
    match mode {
        MoveMode::Abs => false,
        MoveMode::Relative => true,
        MoveMode::Auto => !point_in_primary(point, primary),
    }
}

/// Why a closed-loop move gave up. None of these click; the addon's stage
/// timeouts surface the gap.
#[derive(Debug)]
enum MoveError {
    /// The device did not ack the move (send + one resend): the cursor may
    /// or may not have gone where the delta asked.
    Lost,
    /// The transport broke; retrying cannot help.
    Transport(LinkError),
    /// The device refused the move (a gate is closed).
    Refused(wire::ErrorCode),
    /// The probe moved the cursor not at all: pinned at a desktop edge or
    /// swallowed by interference.
    Pinned,
    /// The nudge budget or time ran out with the cursor outside tolerance.
    Stalled { err: (i32, i32) },
    /// The device is in dry-run: every nudge echoes without moving, so a
    /// closed loop can never converge. Diagnose this instead of reporting a
    /// bogus "pinned cursor".
    DryRun,
}

impl fmt::Display for MoveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lost => f.write_str("ack lost (send + one resend)"),
            Self::Transport(e) => write!(f, "transport failed: {e}"),
            Self::Refused(code) => write!(f, "device refused: {code}"),
            Self::Pinned => f.write_str("cursor did not move (pinned or swallowed)"),
            Self::Stalled { err } => {
                write!(f, "stalled {} px short of the target", err.0.abs().max(err.1.abs()))
            }
            Self::DryRun => f.write_str("device dry-run is on (echoed, nothing actuated)"),
        }
    }
}

/// The visible cursor through the device's relative pointer: read where it
/// is, nudge it by one raw delta (the OS applies ballistics on the way).
trait RelPointer {
    /// Current position in screen coordinates.
    fn read(&self) -> (i32, i32);

    /// Sends one relative delta; `Ok` when the device acked execution.
    fn nudge(&mut self, dx: i16, dy: i16) -> Result<Nudged, MoveError>;
}

/// Whether one nudge physically moved the pointer or was only echoed (the
/// device is in dry-run - the command path ran, nothing moved).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Nudged {
    /// Executed.
    Moved,
    /// Echoed by dry-run.
    Echoed,
}

/// Per-chunk stream allowance for nudge deadlines: the device paces the
/// <=127-count reports at `inter_ms` (4 ms default) and each waits up to
/// one endpoint poll interval (4 ms), so a large delta's ack lands well
/// after the plain ack timeout.
const CHUNK_STREAM_MS: u64 = 8;

/// Settle time after a nudge before the cursor is read again. The ack
/// races the cursor: the vendor channel polls at 1 kHz while the mouse
/// reports cross a 4 ms-poll endpoint and the HID stack, so an immediate
/// `GetCursorPos` samples a half-applied move - measured gains swing
/// wildly (even invert sign), and the loop thrashes (observed live
/// 2026-10-20). A short drain makes every measurement complete.
const REPORT_SETTLE_MS: u64 = 25;

/// The real cursor: `GetCursorPos` reads, `MoveRel` requests for nudges.
struct OsRelPointer<'a> {
    client: &'a mut Client,
    timeout: Duration,
    /// Randomness for the settle gaps (calibration passes pct 0: exact).
    rand: &'a mut OsRandom,
    settle_jitter_pct: u64,
}

impl RelPointer for OsRelPointer<'_> {
    fn read(&self) -> (i32, i32) {
        read_cursor()
    }

    fn nudge(&mut self, dx: i16, dy: i16) -> Result<Nudged, MoveError> {
        let cmd = wire::Cmd::MoveRel { dx, dy };
        // The ack lands only after the paced chunk stream finishes: grow
        // the deadline with the requested delta instead of losing acks for
        // far targets.
        let chunks = u64::from(dx.unsigned_abs().max(dy.unsigned_abs())).div_ceil(127);
        let deadline = self.timeout + Duration::from_millis(chunks * CHUNK_STREAM_MS);
        let reply = match self.client.request(cmd, deadline) {
            Ok(wire::Reply::Ok) => Ok(Nudged::Moved),
            Ok(wire::Reply::Echo) => Ok(Nudged::Echoed),
            Ok(wire::Reply::Err(code)) => Err(MoveError::Refused(code)),
            // A protocol-nonsense reply still proves the device processed
            // the frame; treat the move as executed rather than looping.
            Ok(other) => {
                warn!(command = %cmd, "vkbm unexpected reply {other:?}");
                Ok(Nudged::Moved)
            }
            Err(RequestError::Lost) => Err(MoveError::Lost),
            Err(RequestError::Link(e)) => Err(MoveError::Transport(e)),
        };
        // The cursor is only trustworthy after the report queue drains
        // (see `REPORT_SETTLE_MS`): the ack is not the cursor. The settle
        // gap is jittered so the converge duration is not machine-constant.
        if reply.is_ok() {
            let settle = jitter_ms(REPORT_SETTLE_MS, self.settle_jitter_pct, self.rand);
            thread::sleep(Duration::from_millis(settle));
        }
        reply
    }
}

/// Scales an error by a gain (1.0 = the raw probe), rounded into the wire's
/// i16 delta range.
fn scale_axis(err: i32, gain: f64) -> i16 {
    let scaled = (f64::from(err) / gain).round();
    let clamped = scaled.clamp(f64::from(i16::MIN), f64::from(i16::MAX));
    #[allow(clippy::cast_possible_truncation)] // clamped into i16 above
    {
        clamped as i16
    }
}

/// Adaptive-gain closed loop, hardened against real Windows
/// ballistics after the first off-primary soak (2026-10-20): the probe
/// carries the full remaining error under a 1:1 assumption (machines with
/// ballistics disabled still land in one nudge), then every step
/// re-measures the gain **per axis** and adapts a per-axis step scale that
/// halves on overshoot - the measured gain mispredicting the next report's
/// curve bucket is what made the old single-gain loop ping-pong - and
/// regrows when a step lands short again. Corrections stay capped at twice
/// the remaining error. Failure is honest: past the nudge budget or the
/// deadline the move is abandoned with the measured miss, and the caller
/// refuses to click at an unverified position. Returns the landing
/// position and the nudge count.
fn converge(
    pointer: &mut impl RelPointer,
    target: (i32, i32),
    deadline: Duration,
    seed: &[(i32, f64)],
) -> Result<((i32, i32), u32), MoveError> {
    let started = Instant::now();
    // The probe assumes 1:1; the per-axis measurements replace it. Both
    // axes start from the same calibration seed: the OS gain table is the
    // same for x and y, and runtime samples then specialize each axis.
    let mut gain = (1.0_f64, 1.0_f64);
    let mut profile = (Vec::from(seed), Vec::from(seed));
    // Steps shrink when they overshoot the target (a gain misprediction)
    // and regrow when they land short - the contraction that walks out of
    // ballistics bucket boundaries instead of ping-ponging between them.
    let mut step_scale = (1.0_f64, 1.0_f64);
    let mut pos = pointer.read();
    let mut nudges = 0_u32;
    loop {
        let err = (target.0 - pos.0, target.1 - pos.1);
        if err.0.abs() <= CONVERGE_TOLERANCE_PX && err.1.abs() <= CONVERGE_TOLERANCE_PX {
            return Ok((pos, nudges));
        }
        if nudges >= CONVERGE_MAX_NUDGES || started.elapsed() >= deadline {
            return Err(MoveError::Stalled { err });
        }
        let probe = nudges == 0;
        // The probe carries the full remaining error; corrections are
        // scaled by their own axis' gain and damped step scale.
        let mut delta = if probe {
            (scale_axis(err.0, 1.0), scale_axis(err.1, 1.0))
        } else {
            (
                step_axis(err.0, &profile.0, gain.0, step_scale.0),
                step_axis(err.1, &profile.1, gain.1, step_scale.1),
            )
        };
        // A correction that rounds to zero cannot move the cursor: nudge
        // axes outside tolerance by one count so the loop makes progress
        // (and the next measurement has something to read).
        if delta.0 == 0 && err.0.abs() > CONVERGE_TOLERANCE_PX {
            delta.0 = i16::try_from(err.0.signum()).unwrap_or(0);
        }
        if delta.1 == 0 && err.1.abs() > CONVERGE_TOLERANCE_PX {
            delta.1 = i16::try_from(err.1.signum()).unwrap_or(0);
        }
        let before = pos;
        if matches!(pointer.nudge(delta.0, delta.1)?, Nudged::Echoed) {
            // Dry-run never moves the cursor: bail out with the honest
            // diagnosis instead of measuring a bogus zero gain.
            return Err(MoveError::DryRun);
        }
        pos = pointer.read();
        let moved = (pos.0 - before.0, pos.1 - before.1);
        nudges += 1;
        debug!(
            nudge = nudges,
            sent_x = delta.0,
            sent_y = delta.1,
            moved_x = moved.0,
            moved_y = moved.1,
            err_x = err.0,
            err_y = err.1,
            gain_x = gain.0,
            gain_y = gain.1,
            "converge step"
        );
        if moved.0 == 0 && moved.1 == 0 {
            if probe {
                // The cursor ignored a full-size delta: pinned or blocked.
                return Err(MoveError::Pinned);
            }
            // A sub-count correction can legally round to zero; retry while
            // the budget lasts.
            continue;
        }
        adapt_axis(delta.0, moved.0, err.0, &mut gain.0, &mut step_scale.0, probe, &mut profile.0);
        adapt_axis(delta.1, moved.1, err.1, &mut gain.1, &mut step_scale.1, probe, &mut profile.1);
    }
}

/// The gain measured at the step size most similar to `size`, or
/// `fallback` when the profile is empty.
fn gain_for_size(profile: &[(i32, f64)], size: i32, fallback: f64) -> f64 {
    let mut best: Option<(i32, f64)> = None;
    for &(s, g) in profile {
        let d = (s - size).abs();
        if best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, g));
        }
    }
    best.map_or(fallback, |(_, g)| g)
}

/// Sizes one correction axis: the remaining error scaled by the gain
/// measured at the most similar step size, re-read at the proposed size,
/// under-corrected, bounded - never more than twice the remaining error
/// (plus fixed headroom) even when the profile asks for an i16-span
/// fling. The i32→i16 casts are bounded by construction (the cap and the
/// 1.0 floor), hence the blanket allow.
#[allow(clippy::cast_possible_truncation)]
fn step_axis(err: i32, profile: &[(i32, f64)], fallback: f64, scale: f64) -> i16 {
    let a = err.abs();
    if a == 0 {
        return 0;
    }
    let mut g = gain_for_size(profile, a, fallback);
    let k1 = (f64::from(a) / g * CORRECTION_FACTOR * scale).round().max(1.0) as i32;
    g = gain_for_size(profile, k1, g);
    let k = (f64::from(a) / g * CORRECTION_FACTOR * scale).round().max(1.0) as i32;
    let cap = (2 * a + CORRECTION_HEADROOM_PX).min(i32::from(i16::MAX));
    (err.signum() * k.min(cap)) as i16
}

/// One axis' post-nudge bookkeeping: re-measure the ballistics gain from
/// what the axis just did, and - for gain-scaled corrections only - shrink
/// or regrow the step scale when the step crossed to the other side of the
/// target or landed short again. The probe's overshoot is ballistics
/// working as measured, not a misprediction, so it never damps.
/// Interference (movement against the command) leaves the gain alone -
/// the flip it causes still damps the next step.
fn adapt_axis(
    sent: i16,
    moved: i32,
    err_before: i32,
    gain: &mut f64,
    scale: &mut f64,
    probe: bool,
    profile: &mut Vec<(i32, f64)>,
) {
    if sent != 0 {
        let size = i32::from(sent).abs();
        let sent = f64::from(i32::from(sent));
        let moved = f64::from(moved);
        if moved != 0.0 && moved.signum() == sent.signum() {
            let measured = (moved / sent).clamp(GAIN_MIN, GAIN_MAX);
            *gain = measured;
            // Remember what this step size did, for the next sizing (the
            // oldest sample drops off - only the current regime speaks).
            if profile.len() == PROFILE_SAMPLES {
                profile.remove(0);
            }
            profile.push((size, measured));
        }
    }
    if probe {
        return;
    }
    let err_after = err_before - moved;
    if err_before != 0
        && err_after.abs() > CONVERGE_TOLERANCE_PX
        && err_after.signum() != err_before.signum()
    {
        // The axis crossed to the wrong side by more than the click
        // tolerance: the next report's gain exceeded the prediction.
        *scale = (*scale * OVERSHOOT_SHRINK).max(STEP_SCALE_MIN);
    } else if *scale < 1.0 {
        *scale = 1.0_f64.min(*scale * STEP_REGROW);
    }
}

/// versa-kbm actuator behind the same [`Kbm`] port as the other backends.
pub struct VkbmKbm {
    guard: ActuationGuard,
    client: Client,
    timeout: Duration,
    /// How the pointer is placed before a click.
    move_mode: MoveMode,
    /// Humanized click timing (hold base + jitter percents).
    timing: PressTiming,
    /// Randomness for the humanized timing (hold, settle gaps).
    rand: OsRandom,
    /// Dry-run state at init, so `smoke_echo` restores it faithfully.
    dry_run_at_init: bool,
    /// Calibration seed for the closed loop: (command counts, measured px
    /// per count) from the most recent `calibrate-move` run. Empty when no
    /// calibration file exists - the loop then learns within each move.
    move_profile: Vec<(i32, f64)>,
}

impl VkbmKbm {
    /// Opens the vendor control channel, then handshakes: ping for liveness,
    /// info for the protocol version and the device gate states. The
    /// protocol major must match or init refuses - a silent mismatch would
    /// click wrong. Modes that move relatively additionally require the
    /// firmware minor that introduced the true relative pointer.
    ///
    /// # Errors
    /// Device not found or not openable, no ack to the ping/info, protocol
    /// major mismatch, or a relative mode on firmware without the relative
    /// pointer.
    #[allow(clippy::too_many_arguments)] // one value per config key
    pub fn new(
        vid: u16,
        pid: u16,
        ack_timeout_ms: u64,
        move_mode: MoveMode,
        require_foreground: bool,
        focus_if_needed: bool,
        game: GameWindow,
        move_profile: Vec<(i32, f64)>,
        timing: PressTiming,
    ) -> anyhow::Result<Self> {
        let timeout = Duration::from_millis(ack_timeout_ms);
        let (client, dry_run_at_init) = open_link(vid, pid, timeout, move_mode)?;
        Ok(Self {
            guard: ActuationGuard::new(require_foreground, focus_if_needed, game),
            client,
            timeout,
            move_mode,
            timing,
            rand: OsRandom::from_entropy(),
            dry_run_at_init,
            move_profile,
        })
    }

    /// Dry-run echo round-trip for `doctor --smoke`: turns the device's
    /// dry-run on (a configuration command; no gate involvement), sends one
    /// zero-delta relative move and expects [`wire::Reply::Echo`] - the full
    /// command path runs while nothing is physically actuated, and the zero
    /// delta means not even a pointer report is emitted (unlike the
    /// previous `MoveAbs {0, 0}` probe, which visibly threw the cursor to
    /// the top-left corner if dry-run failed to latch). Restores the prior
    /// dry-run state; a gate rejection (`Disabled`/`Disarmed`) surfaces with
    /// its hint, which is exactly what the doctor should report.
    ///
    /// # Errors
    /// Any request failure, or the move not coming back as an echo.
    pub fn smoke_echo(&mut self) -> anyhow::Result<()> {
        self.client
            .request(wire::Cmd::DryRun { enabled: true }, self.timeout)
            .map_err(request_error)
            .context("enabling device dry-run")?;
        let outcome = self.client.request(wire::Cmd::MoveRel { dx: 0, dy: 0 }, self.timeout);
        // Restore first, report second: a bench session with dry-run on must
        // not silently end up actuating because of a doctor run.
        if !self.dry_run_at_init {
            let _ = self.client.request(wire::Cmd::DryRun { enabled: false }, self.timeout);
        }
        match outcome.map_err(request_error)? {
            wire::Reply::Echo => {
                info!(
                    "smoke: dry-run echo round-trip ok (device command path verified, nothing \
                     actuated)"
                );
                Ok(())
            }
            wire::Reply::Ok => bail!("expected a dry-run echo, got Ok - dry-run did not latch?"),
            wire::Reply::Err(code) => bail!("move refused: {code}"),
            other @ wire::Reply::Info { .. } => bail!("unexpected reply {other:?}"),
        }
    }
}

impl Kbm for VkbmKbm {
    fn press(&mut self, x: i32, y: i32) -> bool {
        let Some((sx, sy)) = self.guard.prepared_point("vkbm press", x, y) else {
            return false;
        };
        // Same TOCTOU re-check as the other backends: the click must not
        // land in whatever window took focus between check and send.
        if !self.guard.still_prepared("vkbm press") {
            return false;
        }
        let (w, h) = screen_size();
        if wants_relative(self.move_mode, (sx, sy), (w, h)) {
            self.press_relative(sx, sy)
        } else {
            self.press_abs(x, y, sx, sy, w, h)
        }
    }

    fn key(&mut self, key: Key) -> bool {
        if !self.guard.prepared_key("vkbm key") {
            return false;
        }
        if !self.guard.still_prepared("vkbm key") {
            return false;
        }
        let cmd = match key {
            Key::Escape => wire::Cmd::KeyTap { usage: USAGE_ESCAPE },
        };
        let outcome = self.client.request(cmd, self.timeout);
        let delivered = matches!(&outcome, Ok(wire::Reply::Ok));
        report_actuation("key", cmd, outcome);
        delivered
    }

    fn cursor(&self) -> (i32, i32) {
        read_cursor()
    }
}

impl VkbmKbm {
    /// The absolute fast path: one `ClickAbs` report. Primary-monitor bound
    /// (Windows normalizes absolute HID mice across the primary only), so a
    /// target outside it is skipped rather than clamped into some other
    /// window; `doctor` reports the placement problem.
    fn press_abs(&mut self, x: i32, y: i32, sx: i32, sy: i32, w: i32, h: i32) -> bool {
        if !point_in_primary((sx, sy), (w, h)) {
            warn!(
                action = "press",
                screen_x = sx,
                screen_y = sy,
                "target outside the primary monitor - move_mode = \"abs\" cannot reach it; \
                 skipping (set move_mode = \"relative\"/\"auto\" or make WoW the primary display)"
            );
            return false;
        }
        let cmd = wire::Cmd::ClickAbs { x: to_abs(sx, w), y: to_abs(sy, h) };
        let outcome = self.client.request(cmd, self.timeout);
        // The echo means the device did NOT click (dry-run): claiming it
        // would poison soak stats.
        let delivered = matches!(&outcome, Ok(wire::Reply::Ok));
        report_actuation("press", cmd, outcome);
        if delivered {
            info!(client_x = x, client_y = y, screen_x = sx, screen_y = sy, "press sent (abs)");
        }
        delivered
    }

    /// The closed-loop path: converge the OS cursor onto the target
    /// with true relative reports, then click through the same pointer.
    /// Works on every monitor; costs a few round-trips instead of one
    /// report, and refuses to click when the move did not verify.
    fn press_relative(&mut self, sx: i32, sy: i32) -> bool {
        // Anchor the press routing to the relative pointer before anything
        // else: a zero delta emits no report but flips the device's "pointer
        // that moved last", so the click can never ride the absolute pointer
        // and yank the cursor to its stale anchor (a zero-error converge
        // sends no relative report of its own).
        let route = wire::Cmd::MoveRel { dx: 0, dy: 0 };
        let outcome = self.client.request(route, self.timeout);
        let routed = matches!(&outcome, Ok(wire::Reply::Ok | wire::Reply::Echo));
        report_actuation("press", route, outcome);
        if !routed {
            return false;
        }
        let (landed, nudges) = {
            let mut pointer = OsRelPointer {
                client: &mut self.client,
                timeout: self.timeout,
                rand: &mut self.rand,
                settle_jitter_pct: self.timing.settle_jitter_pct,
            };
            match converge(&mut pointer, (sx, sy), CONVERGE_DEADLINE, &self.move_profile) {
                Ok(ok) => ok,
                Err(MoveError::Stalled { err }) => {
                    warn!(
                        action = "press",
                        screen_x = sx,
                        screen_y = sy,
                        missed_x = err.0,
                        missed_y = err.1,
                        "relative move did not converge; skipping (never click at an unverified \
                         position)"
                    );
                    return false;
                }
                Err(e) => {
                    warn!(
                        action = "press",
                        screen_x = sx,
                        screen_y = sy,
                        error = %e,
                        "relative move did not converge; skipping (never click at an unverified \
                         position)"
                    );
                    return false;
                }
            }
        };
        // The move took tens of ms; re-check the foreground policy before
        // the click lands (same TOCTOU as everywhere else).
        if !self.guard.still_prepared("vkbm press") {
            return false;
        }
        // A physical mouse bump in the last milliseconds would click at the
        // disturbed position: re-read the real cursor against the verified
        // landing (the converge loop's tolerance).
        let (cur_x, cur_y) = read_cursor();
        if (cur_x - landed.0).abs() > CONVERGE_TOLERANCE_PX
            || (cur_y - landed.1).abs() > CONVERGE_TOLERANCE_PX
        {
            warn!(
                action = "press",
                screen_x = sx,
                screen_y = sy,
                landed_x = landed.0,
                landed_y = landed.1,
                cursor_x = cur_x,
                cursor_y = cur_y,
                "cursor moved after converge; skipping (never click at an unverified position)"
            );
            return false;
        }
        let down = self.press_half(true);
        if matches!(down, HalfOutcome::Executed) {
            let hold = jitter_ms(self.timing.hold_ms, self.timing.hold_jitter_pct, &mut self.rand);
            thread::sleep(Duration::from_millis(hold));
        }
        let up = self.press_half(false);
        // Recovery: whenever the button may be latched - the down landed but
        // the up failed, or either ack was lost (state unknown) - sweep
        // everything released (release-class on the device, always allowed).
        // A refused or echoed down pressed nothing, so no sweep is needed.
        let maybe_latched = matches!(down, HalfOutcome::Executed | HalfOutcome::Lost)
            && !matches!(up, HalfOutcome::Executed);
        if maybe_latched {
            let outcome = self.client.request(wire::Cmd::ReleaseAll, self.timeout);
            report_actuation("press", wire::Cmd::ReleaseAll, outcome);
        }
        let delivered =
            matches!(down, HalfOutcome::Executed) && matches!(up, HalfOutcome::Executed);
        if delivered {
            info!(
                screen_x = sx,
                screen_y = sy,
                landed_x = landed.0,
                landed_y = landed.1,
                nudges,
                "press sent (relative)"
            );
        }
        delivered
    }

    /// One half of a relative-mode click, classified for the recovery and
    /// delivery decisions: a lost ack leaves the button state unknown
    /// (possibly latched), a refusal leaves it untouched, an echo means the
    /// device is in dry-run. The up is attempted even after a lost down: a
    /// release is always safe (release-class on the device) and unlatches
    /// the button whether or not the down landed.
    fn press_half(&mut self, down: bool) -> HalfOutcome {
        let cmd = if down { wire::Cmd::MouseDown } else { wire::Cmd::MouseUp };
        match self.client.request(cmd, self.timeout) {
            Ok(wire::Reply::Ok) => HalfOutcome::Executed,
            Ok(wire::Reply::Echo) => {
                warn!(action = "press", command = %cmd, "vkbm dry-run echo (not actuated)");
                HalfOutcome::Echo
            }
            Ok(wire::Reply::Err(code)) => {
                warn!(action = "press", command = %cmd, "vkbm refused: {code}");
                HalfOutcome::Refused
            }
            Ok(other) => {
                warn!(action = "press", command = %cmd, "vkbm unexpected reply {other:?}");
                HalfOutcome::Refused
            }
            Err(RequestError::Lost) => {
                warn!(action = "press", command = %cmd, "vkbm ack lost - button state unknown");
                HalfOutcome::Lost
            }
            Err(RequestError::Link(e)) => {
                warn!(action = "press", command = %cmd, "vkbm transport failed: {e}");
                HalfOutcome::Lost
            }
        }
    }
}

/// Outcome of one click half (see [`VkbmKbm::press_half`]).
enum HalfOutcome {
    /// The device acked the physical state change.
    Executed,
    /// Dry-run: the device echoed instead of actuating.
    Echo,
    /// The device refused (gate/payload) - nothing was pressed.
    Refused,
    /// No ack: the state change may or may not have landed.
    Lost,
}

/// Current OS cursor position in screen coordinates.
fn read_cursor() -> (i32, i32) {
    let mut pt = POINT { x: 0, y: 0 };
    // SAFETY: plain cursor query into a local struct.
    if unsafe { GetCursorPos(&raw mut pt) }.is_ok() {
        (pt.x, pt.y)
    } else {
        (0, 0)
    }
}

/// Opens the vendor control channel and handshakes: ping for liveness,
/// info for the protocol version and device gate states. The protocol
/// major must match; modes that move relatively additionally require the
/// firmware minor that introduced the true relative pointer. Gate states
/// are logged (they are bench-relevant facts, not errors). Returns the
/// client and whether dry-run was on at init.
fn open_link(
    vid: u16,
    pid: u16,
    timeout: Duration,
    move_mode: MoveMode,
) -> anyhow::Result<(Client, bool)> {
    let api = HidApi::new().context("hidapi init")?;
    let device_info = find_device(&api, vid, pid).with_context(|| {
        format!("no versa-kbm vendor channel on {vid:04x}:{pid:04x} (plugged in? flashed?)")
    })?;
    let device = api.open_path(device_info.path()).context("opening the versa-kbm device")?;
    let mut client = Client::new(device);

    client
        .request(wire::Cmd::Ping, timeout)
        .map_err(request_error)
        .context("versa-kbm did not ack the ping")?;
    let reply = client
        .request(wire::Cmd::Info, timeout)
        .map_err(request_error)
        .context("versa-kbm did not answer the info query")?;
    let wire::Reply::Info { major, minor, flags } = reply else {
        bail!("versa-kbm answered info with {reply:?}");
    };
    if major != PROTOCOL_MAJOR {
        bail!(
            "versa-kbm protocol mismatch: firmware v{major}.{minor}, this sphinx speaks \
             v{PROTOCOL_MAJOR}.x - flash a matching firmware"
        );
    }
    if relative_requested(move_mode) && minor < RELATIVE_FW_MINOR {
        bail!(
            "versa-kbm firmware v{major}.{minor} predates the true relative pointer (needs \
             0.5+); flash a newer firmware or set backend.vkbm.move_mode = \"abs\""
        );
    }
    let dry_run_at_init = flags & wire::info_flags::DRY_RUN != 0;
    let switch = flags & wire::info_flags::SWITCH_ON != 0;
    let armed = flags & wire::info_flags::ARMED != 0;
    if !switch {
        warn!(
            "versa-kbm hardware switch is OFF - nothing will actuate until someone taps \
             BOOTSEL on the board (that is the point of the switch)"
        );
    } else if !armed {
        warn!("versa-kbm is disarmed - run `vkbm arm` to re-enable actuation");
    }
    if dry_run_at_init {
        warn!("versa-kbm dry-run is ON - actuation is echoed, not executed");
    }
    info!(
        firmware = format!("v{major}.{minor}"),
        vid = format!("{vid:04x}"),
        pid = format!("{pid:04x}"),
        switch,
        armed,
        dry_run = dry_run_at_init,
        "versa-kbm link up (ping acked)"
    );
    Ok((client, dry_run_at_init))
}

/// Command sizes swept by [`calibrate_move`], in counts: dense across the
/// single-report ramp (1..=127 counts is where the gain bends), sparse
/// above - larger commands are chunked into <=127-count reports by the
/// device, so their per-command gain is the flat large-report regime.
const CALIBRATION_SIZES: [i32; 17] =
    [1, 2, 3, 5, 8, 12, 18, 27, 40, 60, 90, 127, 180, 270, 405, 608, 912];

/// Keep-out from the virtual desktop edges when sweeping, in px.
const SWEEP_EDGE_MARGIN: i32 = 8;

/// Virtual desktop bounds (left, top, right, bottom) in screen px. The
/// `SM_C*`/`SM_Y*` metrics are the origin and the *size*, so the edges are
/// computed here - consuming the sizes as edge coordinates only works when
/// the desktop origin is (0, 0), which a monitor left of / above the primary
/// breaks.
fn virtual_desktop_bounds() -> (i32, i32, i32, i32) {
    // SAFETY: plain metric queries; no pointers, cannot fail.
    unsafe {
        let (left, top) =
            (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
        (
            left,
            top,
            left + GetSystemMetrics(SM_CXVIRTUALSCREEN),
            top + GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

/// Outward sweep direction for a size-`k` probe at `x`: right when the
/// outward step fits inside the desktop, else left, else `None` when neither
/// side has room (the caller skips that size).
#[must_use]
fn sweep_dir(x: i32, k: i32, left: i32, right: i32) -> Option<i32> {
    if x + k <= right - SWEEP_EDGE_MARGIN {
        Some(1)
    } else if x - k >= left + SWEEP_EDGE_MARGIN {
        Some(-1)
    } else {
        None
    }
}

/// Sweeps [`CALIBRATION_SIZES`] around the current cursor position and
/// measures what the OS does to each command: move out by `k` counts,
/// measure the landed px, move back (the return move is the same size
/// and averaged in). Ping-pong keeps the cursor near the start; a size
/// whose outward step would cross the desktop edge is measured in the
/// other direction, or skipped when neither fits. Sizes whose sample
/// moved 0 px (quantization at 1-2 counts) are dropped. Returns
/// `(counts, px-per-count)` pairs, gains clamped to `GAIN_MIN..=GAIN_MAX`.
///
/// # Errors
/// Link/handshake failures, a gate state that blocks actuation, or a lost
/// ack mid-sweep.
pub fn calibrate_move(vid: u16, pid: u16, ack_timeout_ms: u64) -> anyhow::Result<Vec<(i32, f64)>> {
    let timeout = Duration::from_millis(ack_timeout_ms);
    let (mut client, dry_run_at_init) = open_link(vid, pid, timeout, MoveMode::Relative)?;
    if dry_run_at_init {
        bail!("versa-kbm dry-run is on - run `vkbm dry-run off` before calibrating");
    }
    let mut pointer = OsRelPointer {
        client: &mut client,
        timeout,
        rand: &mut OsRandom::from_entropy(),
        settle_jitter_pct: 0,
    };
    let (left, _, right, _) = virtual_desktop_bounds();
    info!(
        sizes = CALIBRATION_SIZES.len(),
        bounds = ?(left, right),
        "calibrate-move: sweeping (the cursor will wiggle around its current spot)"
    );
    let mut samples = Vec::with_capacity(CALIBRATION_SIZES.len());
    for &k in &CALIBRATION_SIZES {
        let (x, _) = pointer.read();
        // Sweep outward on x where there is room, else in the other
        // direction; y stays put (the OS gain table is the same for both
        // axes, so one axis measures it for both).
        let Some(dir) = sweep_dir(x, k, left, right) else {
            warn!(counts = k, "calibrate-move: no room on either side, skipping");
            continue;
        };
        let mut gains = Vec::new();
        for &d in &[dir, -dir] {
            let before = pointer.read();
            if matches!(
                pointer.nudge(i16::try_from(d * k).unwrap_or(0), 0).map_err(|e| anyhow!("{e}"))?,
                Nudged::Echoed
            ) {
                bail!("versa-kbm dry-run turned on mid-sweep");
            }
            let after = pointer.read();
            let moved = (after.0 - before.0).abs();
            if moved > 0 {
                gains.push(f64::from(moved) / f64::from(k));
            }
        }
        if gains.is_empty() {
            debug!(counts = k, "calibrate-move: sample moved 0 px, dropped");
            continue;
        }
        // At most two measurements per size (out + back).
        let gain = gains.iter().sum::<f64>()
            / match gains.len() {
                1 => 1.0_f64,
                _ => 2.0,
            };
        let gain = gain.clamp(GAIN_MIN, GAIN_MAX);
        info!(counts = k, px_per_count = gain, "calibrate-move: sample");
        samples.push((k, gain));
    }
    Ok(samples)
}

/// The `move-profile.toml` document: parallel arrays of command size
/// (counts) and measured px-per-count, written by `sphinx calibrate-move`.
#[derive(Serialize, Deserialize)]
struct MoveProfileFile {
    counts: Vec<i32>,
    gain: Vec<f64>,
}

/// Loads a move-calibration file written by `calibrate-move`. A missing
/// file is an empty profile (the loop learns live); a corrupt one warns
/// and degrades to empty rather than refusing to start.
pub fn load_move_profile(path: &Path) -> Vec<(i32, f64)> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            warn!(
                path = %path.display(),
                error = %e,
                "move profile unreadable; starting uncalibrated"
            );
            return Vec::new();
        }
    };
    match toml::from_str::<MoveProfileFile>(&raw) {
        Ok(file) if file.counts.len() == file.gain.len() => {
            file.counts.into_iter().zip(file.gain).collect()
        }
        Ok(_) => {
            warn!(path = %path.display(), "move profile arrays disagree in length; ignored");
            Vec::new()
        }
        Err(e) => {
            warn!(path = %path.display(), error = %e, "move profile unparsable; ignored");
            Vec::new()
        }
    }
}

/// Writes a move-calibration file (see [`load_move_profile`]).
///
/// # Errors
/// Any filesystem failure on write/serialize.
pub fn save_move_profile(path: &Path, samples: &[(i32, f64)]) -> anyhow::Result<()> {
    let file = MoveProfileFile {
        counts: samples.iter().map(|&(c, _)| c).collect(),
        gain: samples.iter().map(|&(_, g)| g).collect(),
    };
    let raw = toml::to_string_pretty(&file).context("serializing the move profile")?;
    std::fs::write(path, raw).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        converge,
        initial_seq,
        issue,
        load_move_profile,
        point_in_primary,
        relative_requested,
        save_move_profile,
        scale_axis,
        step_axis,
        sweep_dir,
        to_abs,
        wait_ack,
        wants_relative,
        wire,
        Client,
        MoveError,
        Nudged,
        RelPointer,
        ABS_MAX,
        CONVERGE_DEADLINE,
        CONVERGE_MAX_NUDGES,
        CONVERGE_TOLERANCE_PX,
        CORRECTION_HEADROOM_PX,
        GAIN_MIN,
        STEP_SCALE_MIN,
    };
    use crate::{
        adapters::kbm::vkbm::{
            Link,
            LinkError,
            RequestError,
        },
        config::MoveMode,
    };

    const TIMEOUT: Duration = Duration::from_millis(250);

    type Replies =
        std::rc::Rc<std::cell::RefCell<std::collections::VecDeque<Result<[u8; 64], LinkError>>>>;

    #[derive(Default, Clone)]
    struct FakeLink {
        sent: std::rc::Rc<std::cell::RefCell<Vec<[u8; 64]>>>,
        replies: Replies,
    }

    impl FakeLink {
        fn sent(&self) -> usize {
            self.sent.borrow().len()
        }

        fn sent_seq(&self, attempt: usize) -> Option<u8> {
            self.sent.borrow().get(attempt).map(|frame| frame[0])
        }
    }

    impl Link for FakeLink {
        fn send(&mut self, report: &[u8; 64]) -> Result<(), LinkError> {
            self.sent.borrow_mut().push(*report);
            Ok(())
        }

        fn recv(&mut self, _timeout: Duration) -> Result<[u8; 64], LinkError> {
            self.replies.borrow_mut().pop_front().unwrap_or(Err(LinkError::Timeout))
        }
    }

    fn ack_for(seq: u8, reply: wire::Reply) -> [u8; 64] {
        let mut buf = [0u8; 64];
        wire::encode_reply(seq, reply, &mut buf);
        buf
    }

    fn frame_for(seq: u8) -> [u8; 64] {
        let mut buf = [0u8; 64];
        wire::encode(wire::Cmd::Ping, seq, &mut buf);
        buf
    }

    // --- codec ---

    #[test]
    fn encodes_header_and_zero_padding() {
        let mut buf = [1u8; 64];
        wire::encode(wire::Cmd::Ping, 7, &mut buf);
        assert_eq!(buf[0], 7);
        assert_eq!(buf[1], 0x01, "ping tag");
        assert!(buf[2..].iter().all(|&b| b == 0));
    }

    #[test]
    fn encodes_relative_moves_little_endian() {
        let mut out = [0u8; 64];
        wire::encode(wire::Cmd::MoveRel { dx: -300, dy: 400 }, 7, &mut out);
        assert_eq!(out[0], 7);
        assert_eq!(out[1], 0x21);
        assert_eq!(&out[2..4], &(-300i16).to_le_bytes());
        assert_eq!(&out[4..6], &(400i16).to_le_bytes());
        assert!(out[6..].iter().all(|&b| b == 0), "rest must stay zero padding");
    }

    #[test]
    fn encodes_mouse_down_up_as_left_button() {
        let mut out = [0u8; 64];
        wire::encode(wire::Cmd::MouseDown, 1, &mut out);
        assert_eq!(out[1], 0x22);
        assert_eq!(out[2], wire::BUTTON_LEFT);
        wire::encode(wire::Cmd::MouseUp, 2, &mut out);
        assert_eq!(out[1], 0x23);
        assert_eq!(out[2], wire::BUTTON_LEFT);
        assert!(out[3..].iter().all(|&b| b == 0));
    }

    #[test]
    fn encodes_keytap_and_release_all() {
        let mut buf = [0u8; 64];
        wire::encode(wire::Cmd::KeyTap { usage: 0x2C }, 7, &mut buf);
        assert_eq!(buf[0], 7);
        assert_eq!(buf[1], 0x12, "key-tap tag");
        assert_eq!(buf[2], 0x2C, "usage byte");
        assert!(buf[3..].iter().all(|&b| b == 0));

        wire::encode(wire::Cmd::ReleaseAll, 9, &mut buf);
        assert_eq!(buf[0], 9);
        assert_eq!(buf[1], 0x30, "release-all tag");
        assert!(buf[2..].iter().all(|&b| b == 0));
    }

    #[test]
    fn encodes_absolute_moves_little_endian() {
        let mut buf = [0u8; 64];
        wire::encode(wire::Cmd::ClickAbs { x: 16_384, y: 4_096 }, 9, &mut buf);
        assert_eq!(buf[1], 0x24, "click-abs tag");
        assert_eq!(&buf[2..4], &16_384u16.to_le_bytes());
        assert_eq!(&buf[4..6], &4_096u16.to_le_bytes());
    }

    #[test]
    fn encodes_dry_run_strictly() {
        let mut on = [0u8; 64];
        wire::encode(wire::Cmd::DryRun { enabled: true }, 1, &mut on);
        assert_eq!(on[1], 0x34);
        assert_eq!(on[2], 1);
        let mut off = [0u8; 64];
        wire::encode(wire::Cmd::DryRun { enabled: false }, 1, &mut off);
        assert_eq!(off[2], 0);
    }

    #[test]
    fn replies_roundtrip() {
        let samples = [
            wire::Reply::Ok,
            wire::Reply::Echo,
            wire::Reply::Info { major: 0, minor: 4, flags: 0x07 },
            wire::Reply::Err(wire::ErrorCode::UnknownCommand),
            wire::Reply::Err(wire::ErrorCode::BadPayload),
            wire::Reply::Err(wire::ErrorCode::Disarmed),
            wire::Reply::Err(wire::ErrorCode::NotImplemented),
            wire::Reply::Err(wire::ErrorCode::KeyboardFull),
            wire::Reply::Err(wire::ErrorCode::Disabled),
        ];
        for reply in samples {
            let frame = ack_for(7, reply);
            assert_eq!(wire::decode_reply(&frame), Ok((7, reply)));
        }
    }

    #[test]
    fn rejects_junk_replies() {
        assert_eq!(wire::decode_reply(&[]), Err(()));
        assert_eq!(wire::decode_reply(&[7]), Err(()));
        let mut unknown = [0u8; 64];
        unknown[1] = 0x7F;
        assert_eq!(wire::decode_reply(&unknown), Err(()));
        let mut bad_err = [0u8; 64];
        bad_err[1] = 0xFF;
        bad_err[2] = 0x40;
        assert_eq!(wire::decode_reply(&bad_err), Err(()));
        let mut dirty_ok = [0u8; 64];
        dirty_ok[1] = 0x00;
        dirty_ok[5] = 1;
        assert_eq!(wire::decode_reply(&dirty_ok), Err(()));
    }

    // --- ack policy ---

    #[test]
    fn ack_on_first_attempt() {
        let mut link = FakeLink {
            replies: std::rc::Rc::new(std::cell::RefCell::new(std::collections::VecDeque::from([
                Ok(ack_for(1, wire::Reply::Ok)),
            ]))),
            ..FakeLink::default()
        };
        let reply = issue(&mut link, &frame_for(1), 1, TIMEOUT);
        assert!(matches!(reply, Ok(wire::Reply::Ok)));
        assert_eq!(link.sent(), 1, "no resend on a clean ack");
    }

    #[test]
    fn lost_ack_is_resent_once_with_the_same_seq() {
        // The device dedups by seq: the resend must carry the identical seq
        // so a cached reply (not a second execution) answers it.
        let mut link = FakeLink {
            replies: std::rc::Rc::new(std::cell::RefCell::new(std::collections::VecDeque::from([
                Err(LinkError::Timeout),
                Ok(ack_for(1, wire::Reply::Ok)),
            ]))),
            ..FakeLink::default()
        };
        let reply = issue(&mut link, &frame_for(1), 1, TIMEOUT);
        assert!(matches!(reply, Ok(wire::Reply::Ok)));
        assert_eq!(link.sent(), 2, "exactly one resend");
        assert_eq!(link.sent_seq(0), link.sent_seq(1), "resend keeps the seq");
    }

    #[test]
    fn double_timeout_gives_up() {
        let mut link = FakeLink {
            replies: std::rc::Rc::new(std::cell::RefCell::new(std::collections::VecDeque::from([
                Err(LinkError::Timeout),
                Err(LinkError::Timeout),
            ]))),
            ..FakeLink::default()
        };
        let reply = issue(&mut link, &frame_for(1), 1, TIMEOUT);
        assert!(matches!(reply, Err(RequestError::Lost)));
        assert_eq!(link.sent(), 2);
    }

    #[test]
    fn transport_error_is_never_retried() {
        let mut link = FakeLink {
            replies: std::rc::Rc::new(std::cell::RefCell::new(std::collections::VecDeque::from([
                Err(LinkError::Io("gone".to_string())),
            ]))),
            ..FakeLink::default()
        };
        let reply = issue(&mut link, &frame_for(1), 1, TIMEOUT);
        assert!(matches!(reply, Err(RequestError::Link(_))));
        assert_eq!(link.sent(), 1, "no resend after a transport failure");
    }

    #[test]
    fn stale_replies_are_discarded() {
        let mut link = FakeLink {
            replies: std::rc::Rc::new(std::cell::RefCell::new(std::collections::VecDeque::from([
                Ok(ack_for(9, wire::Reply::Ok)),
                Ok(ack_for(1, wire::Reply::Ok)),
            ]))),
            ..FakeLink::default()
        };
        let reply = issue(&mut link, &frame_for(1), 1, TIMEOUT);
        assert!(matches!(reply, Ok(wire::Reply::Ok)));
        assert_eq!(link.sent(), 1, "a stale ack must not consume the resend");
    }

    #[test]
    fn junk_reply_flood_times_out_instead_of_looping() {
        let flood: std::collections::VecDeque<_> =
            (0..32).map(|_| Ok(ack_for(9, wire::Reply::Ok))).collect();
        let mut link = FakeLink {
            replies: std::rc::Rc::new(std::cell::RefCell::new(flood)),
            ..FakeLink::default()
        };
        let reply = issue(&mut link, &frame_for(1), 1, TIMEOUT);
        assert!(matches!(reply, Err(RequestError::Lost)));
        assert_eq!(link.sent(), 2, "the resend still happens once, then give up");
    }

    #[test]
    fn wait_ack_accepts_an_echo_as_the_ack() {
        let mut link = FakeLink {
            replies: std::rc::Rc::new(std::cell::RefCell::new(std::collections::VecDeque::from([
                Ok(ack_for(3, wire::Reply::Echo)),
            ]))),
            ..FakeLink::default()
        };
        let reply = wait_ack(&mut link, 3, TIMEOUT);
        assert!(matches!(reply, Ok(wire::Reply::Echo)));
    }

    // --- client ---

    #[test]
    fn initial_seq_stays_in_host_range() {
        // The wire contract reserves 0 as "never executed" on the device;
        // the seed must never produce it.
        for _ in 0..256 {
            let seq = initial_seq();
            assert!((1..=u8::MAX).contains(&seq), "seeded seq {seq} outside 1..=255");
        }
    }

    #[test]
    fn client_advances_sequence_numbers() {
        let fake = FakeLink {
            replies: std::rc::Rc::new(std::cell::RefCell::new(std::collections::VecDeque::from([
                Ok(ack_for(1, wire::Reply::Ok)),
                Ok(ack_for(2, wire::Reply::Ok)),
            ]))),
            ..FakeLink::default()
        };
        let mut client = Client { link: Box::new(fake.clone()), next_seq: 1 };
        let _ = client.request(wire::Cmd::Ping, TIMEOUT);
        let _ = client.request(wire::Cmd::Ping, TIMEOUT);
        assert_eq!(fake.sent_seq(0), Some(1));
        assert_eq!(fake.sent_seq(1), Some(2));
    }

    #[test]
    fn client_wraps_sequence_numbers_past_u8_max() {
        let fake = FakeLink {
            replies: std::rc::Rc::new(std::cell::RefCell::new(std::collections::VecDeque::from([
                Ok(ack_for(u8::MAX, wire::Reply::Ok)),
                Ok(ack_for(1, wire::Reply::Ok)),
            ]))),
            ..FakeLink::default()
        };
        let mut client = Client { link: Box::new(fake.clone()), next_seq: u8::MAX };
        let _ = client.request(wire::Cmd::Ping, TIMEOUT);
        let _ = client.request(wire::Cmd::Ping, TIMEOUT);
        assert_eq!(fake.sent_seq(0), Some(u8::MAX));
        assert_eq!(fake.sent_seq(1), Some(1), "seq wraps MAX -> 1");
    }

    // --- normalization ---

    #[test]
    fn to_abs_maps_endpoints_exactly() {
        assert_eq!(to_abs(0, 1920), 0);
        assert_eq!(to_abs(1919, 1920), ABS_MAX);
        assert_eq!(to_abs(0, 1080), 0);
        assert_eq!(to_abs(1079, 1080), ABS_MAX);
    }

    #[test]
    fn to_abs_clamps_out_of_range_pixels() {
        assert_eq!(to_abs(-5, 1920), 0);
        assert_eq!(to_abs(5_000, 1920), ABS_MAX);
    }

    #[test]
    fn to_abs_handles_degenerate_dimensions() {
        assert_eq!(to_abs(5, 1), 0);
        assert_eq!(to_abs(5, 0), 0);
    }

    #[test]
    fn to_abs_is_monotonic() {
        let mut last = 0;
        for px in 0..1920 {
            let abs = to_abs(px, 1920);
            assert!(abs >= last, "abs must not decrease at px={px}");
            last = abs;
        }
    }

    // --- primary-monitor gate ---

    #[test]
    fn points_on_the_primary_are_reachable() {
        assert!(point_in_primary((0, 0), (1920, 1080)));
        assert!(point_in_primary((1919, 1079), (1920, 1080)));
        assert!(point_in_primary((960, 540), (1920, 1080)));
    }

    #[test]
    fn points_off_the_primary_are_refused() {
        // A right-of-primary monitor starts at x = 1920: the second-screen
        // case that mis-clicked before the gate existed.
        assert!(!point_in_primary((1920, 400), (1920, 1080)));
        assert!(!point_in_primary((2500, 700), (1920, 1080)));
        assert!(!point_in_primary((100, 1080), (1920, 1080)));
        assert!(!point_in_primary((-1, 5), (1920, 1080)));
        assert!(!point_in_primary((5, -1), (1920, 1080)));
    }

    // --- movement modes ---

    #[test]
    fn relative_mode_moves_everywhere_and_abs_only_on_the_primary() {
        let off_primary = (2500, 400);
        assert!(wants_relative(MoveMode::Relative, off_primary, (1920, 1080)));
        assert!(wants_relative(MoveMode::Relative, (960, 540), (1920, 1080)));
        assert!(!wants_relative(MoveMode::Abs, (960, 540), (1920, 1080)));
        assert!(!wants_relative(MoveMode::Abs, off_primary, (1920, 1080)));
        // Auto: the absolute fast path on the primary, closed loop beyond.
        assert!(!wants_relative(MoveMode::Auto, (960, 540), (1920, 1080)));
        assert!(wants_relative(MoveMode::Auto, off_primary, (1920, 1080)));
        // Only the relative-moving modes need the fw 0.5 capability.
        assert!(relative_requested(MoveMode::Relative));
        assert!(relative_requested(MoveMode::Auto));
        assert!(!relative_requested(MoveMode::Abs));
    }

    // --- closed loop ---

    /// A cursor world for converge tests: a position plus a ballistics gain
    /// function applied per sent axis. No hardware, no Windows.
    struct FakePointer {
        pos: (i32, i32),
        gain: fn(i32) -> f64,
        /// Nudges the cursor ignores (pinned/blocked cursor).
        inert: bool,
        /// Nudges are echoed instead of executed (device dry-run).
        dry_run: bool,
        /// Error returned instead of moving (e.g. lost acks).
        fail: Option<MoveError>,
        nudges: Vec<(i16, i16)>,
    }

    impl FakePointer {
        fn at(pos: (i32, i32), gain: fn(i32) -> f64) -> Self {
            Self { pos, gain, inert: false, dry_run: false, fail: None, nudges: Vec::new() }
        }
    }

    impl RelPointer for FakePointer {
        fn read(&self) -> (i32, i32) {
            self.pos
        }

        #[allow(clippy::cast_possible_truncation)] // test world is unbounded
        fn nudge(&mut self, dx: i16, dy: i16) -> Result<Nudged, MoveError> {
            self.nudges.push((dx, dy));
            if let Some(e) = self.fail.take() {
                return Err(e);
            }
            if self.dry_run {
                return Ok(Nudged::Echoed);
            }
            if !self.inert {
                self.pos.0 += (f64::from(dx) * (self.gain)(i32::from(dx))).round() as i32;
                self.pos.1 += (f64::from(dy) * (self.gain)(i32::from(dy))).round() as i32;
            }
            Ok(Nudged::Moved)
        }
    }

    fn unit(_: i32) -> f64 {
        1.0
    }

    fn half(_: i32) -> f64 {
        0.5
    }

    /// High gain for large sends, 1:1 for corrections - the shape of real
    /// Windows ballistics (gain is a function of speed).
    fn epp_like(speed: i32) -> f64 {
        match speed.abs() {
            0..=3 => 1.0,
            4..=11 => 1.5,
            12..=47 => 3.0,
            _ => 6.0,
        }
    }

    /// The pointer-speed slider's max uniform multiplier (~10x). Physical
    /// note: `MoveRel` reports never exceed 127 counts (the device chunks
    /// them), so an OS gain can only be a function of report sizes up to
    /// 127 - a size-dependent cliff beyond that cannot happen on the wire.
    fn uniform_ten(_: i32) -> f64 {
        10.0
    }

    /// A flat 20x on every send including 1-count corrections: the
    /// pathological setting where 1 px can never be hit.
    fn flat_twenty(_: i32) -> f64 {
        20.0
    }

    #[test]
    fn converges_in_one_probe_at_unit_gain() -> Result<(), MoveError> {
        let mut p = FakePointer::at((100, 100), unit);
        let (pos, nudges) = converge(&mut p, (500, 500), CONVERGE_DEADLINE, &[])?;
        assert_eq!(pos, (500, 500));
        assert_eq!(nudges, 1, "1:1 ballistics: the probe lands exactly");
        Ok(())
    }

    #[test]
    fn converges_at_half_gain() -> Result<(), MoveError> {
        let mut p = FakePointer::at((0, 0), half);
        let (pos, nudges) = converge(&mut p, (400, 200), CONVERGE_DEADLINE, &[])?;
        assert!(
            (pos.0 - 400).abs() <= CONVERGE_TOLERANCE_PX
                && (pos.1 - 200).abs() <= CONVERGE_TOLERANCE_PX,
            "got {pos:?} after {nudges} nudges"
        );
        assert!(nudges <= CONVERGE_MAX_NUDGES, "probe + capped corrections: got {nudges}");
        Ok(())
    }

    #[test]
    fn converges_at_max_pointer_speed_gain() -> Result<(), MoveError> {
        let mut p = FakePointer::at((0, 0), uniform_ten);
        let (pos, nudges) = converge(&mut p, (1000, 300), CONVERGE_DEADLINE, &[])?;
        assert!(
            (pos.0 - 1000).abs() <= CONVERGE_TOLERANCE_PX
                && (pos.1 - 300).abs() <= CONVERGE_TOLERANCE_PX,
            "got {pos:?} after {nudges} nudges"
        );
        assert!(nudges <= 8, "under-correction costs a few nudges, got {nudges}");
        Ok(())
    }

    #[test]
    fn converges_through_epp_style_curve() -> Result<(), MoveError> {
        let mut p = FakePointer::at((0, 0), epp_like);
        let (pos, nudges) = converge(&mut p, (1000, 400), CONVERGE_DEADLINE, &[])?;
        assert!(
            (pos.0 - 1000).abs() <= CONVERGE_TOLERANCE_PX
                && (pos.1 - 400).abs() <= CONVERGE_TOLERANCE_PX,
            "lands within the click tolerance, got {pos:?}"
        );
        assert!(nudges <= CONVERGE_MAX_NUDGES);
        Ok(())
    }

    /// The device chunks any command into <=127-count reports, so the OS
    /// ballistics bucket every large command identically; only single small
    /// reports sit in lower buckets. Modeled as the gain of the clamped
    /// command size.
    fn chunk_bucketed(speed: i32) -> f64 {
        epp_like(speed.clamp(-127, 127))
    }

    #[test]
    fn converges_from_afar_through_chunked_ballistics() -> Result<(), MoveError> {
        // The live off-primary failure (2026-10-20): a distant probe
        // overshot under ballistics and the old 4-nudge / 150 ms budget
        // stalled mid walk-back, leaving the cursor ~190 px off the target.
        // The full-distance probe plus per-axis adapted corrections must
        // still land from a desktop away.
        let mut p = FakePointer::at((-4000, -2000), chunk_bucketed);
        let (pos, nudges) = converge(&mut p, (330, 450), CONVERGE_DEADLINE, &[])?;
        assert!(
            (pos.0 - 330).abs() <= CONVERGE_TOLERANCE_PX
                && (pos.1 - 450).abs() <= CONVERGE_TOLERANCE_PX,
            "got {pos:?} after {nudges} nudges"
        );
        assert!(nudges <= CONVERGE_MAX_NUDGES);
        Ok(())
    }

    /// A narrow amplified band the corrections must cross: the step sized
    /// by one bucket's gain lands in the other, the classic shape that made
    /// the old single-gain loop ping-pong past the target.
    fn bouncing_bucket(speed: i32) -> f64 {
        match speed.abs() {
            5..=60 => 3.0,
            _ => 1.5,
        }
    }

    #[test]
    fn overshooting_bucket_boundary_damps_then_lands() -> Result<(), MoveError> {
        let mut p = FakePointer::at((0, 0), bouncing_bucket);
        let (pos, nudges) = converge(&mut p, (100, 0), CONVERGE_DEADLINE, &[])?;
        assert!((pos.0 - 100).abs() <= CONVERGE_TOLERANCE_PX, "got {pos:?} after {nudges} nudges");
        assert!(
            p.nudges.iter().any(|&(dx, _)| i32::from(dx).abs() < 50),
            "the damped follow-up step must shrink, not re-fling"
        );
        Ok(())
    }

    /// A live-world-shaped curve (the shape the bench sweep measured).
    fn live_shaped(speed: i32) -> f64 {
        match speed.abs() {
            0..=4 => 0.5,
            5..=11 => 1.0,
            12..=47 => 1.65,
            _ => 2.5,
        }
    }

    #[test]
    fn move_profile_roundtrips_through_toml() -> anyhow::Result<()> {
        let samples = vec![(1_i32, 0.5), (127, 2.5), (912, 2.4)];
        let path =
            std::env::temp_dir().join(format!("sphinx-move-profile-{}.toml", std::process::id()));
        save_move_profile(&path, &samples)?;
        let loaded = load_move_profile(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded, samples);
        Ok(())
    }

    #[test]
    fn move_profile_unparsable_degrades_to_empty() {
        let path = std::env::temp_dir()
            .join(format!("sphinx-move-profile-corrupt-{}.toml", std::process::id()));
        let written = std::fs::write(&path, "counts = [1, 2\ngain = nope");
        assert!(written.is_ok(), "temp write failed");
        let loaded = load_move_profile(&path);
        let _ = std::fs::remove_file(&path);
        assert!(loaded.is_empty(), "a corrupt profile must be ignored, got {loaded:?}");
    }

    #[test]
    fn move_profile_length_mismatch_degrades_to_empty() {
        let path = std::env::temp_dir()
            .join(format!("sphinx-move-profile-mismatch-{}.toml", std::process::id()));
        let written = std::fs::write(&path, "counts = [1, 2]\ngain = [1.0]\n");
        assert!(written.is_ok(), "temp write failed");
        let loaded = load_move_profile(&path);
        let _ = std::fs::remove_file(&path);
        assert!(loaded.is_empty(), "mismatched arrays must be ignored, got {loaded:?}");
    }

    // --- calibration sweep bounds ---

    #[test]
    fn sweep_direction_prefers_right_when_it_fits() {
        assert_eq!(sweep_dir(100, 50, 0, 1920), Some(1));
        assert_eq!(sweep_dir(100, 50, -1920, 1920), Some(1), "non-zero origin desktop");
    }

    #[test]
    fn sweep_direction_falls_back_to_left_at_the_edge() {
        assert_eq!(sweep_dir(1900, 50, 0, 1920), Some(-1));
        assert_eq!(sweep_dir(1900, 50, -1920, 1920), Some(-1));
    }

    #[test]
    fn sweep_direction_skips_when_neither_side_fits() {
        assert_eq!(sweep_dir(10, 50, 0, 20), None);
    }

    #[test]
    fn calibration_seed_lands_the_first_corrections() -> Result<(), MoveError> {
        // What `calibrate-move` writes: the curve, pre-measured. The very
        // first correction must already size right - no learning nudges.
        let mut p = FakePointer::at((0, 0), live_shaped);
        let seed = [(127_i32, 2.5), (64, 2.5), (20, 1.65), (8, 1.0), (3, 0.5)];
        let (pos, nudges) = converge(&mut p, (1000, 400), CONVERGE_DEADLINE, &seed)?;
        assert!(
            (pos.0 - 1000).abs() <= CONVERGE_TOLERANCE_PX
                && (pos.1 - 400).abs() <= CONVERGE_TOLERANCE_PX,
            "got {pos:?} after {nudges} nudges"
        );
        assert!(nudges <= 5, "a seeded curve must not need learning nudges: {nudges}");
        Ok(())
    }

    #[test]
    fn flat_extreme_gain_stalls_instead_of_clicking_wild() {
        // 20x on 1-count sends means sub-pixel targets are unreachable; the
        // loop must refuse to click rather than guess.
        let mut p = FakePointer::at((0, 0), flat_twenty);
        let outcome = converge(&mut p, (3, 0), CONVERGE_DEADLINE, &[]);
        assert!(matches!(outcome, Err(MoveError::Stalled { .. })));
    }

    #[test]
    fn pinned_cursor_reports_pinned() {
        let mut p = FakePointer::at((0, 0), unit);
        p.inert = true;
        let outcome = converge(&mut p, (300, 300), CONVERGE_DEADLINE, &[]);
        assert!(matches!(outcome, Err(MoveError::Pinned)));
    }

    #[test]
    fn lost_ack_during_move_aborts_the_click() {
        let mut p = FakePointer::at((0, 0), unit);
        p.fail = Some(MoveError::Lost);
        let outcome = converge(&mut p, (300, 300), CONVERGE_DEADLINE, &[]);
        assert!(matches!(outcome, Err(MoveError::Lost)));
    }

    #[test]
    fn dry_run_echo_is_diagnosed_not_pinned() {
        // A device in dry-run echoes every nudge without moving: the loop
        // must say "dry-run", not misreport a pinned cursor.
        let mut p = FakePointer::at((0, 0), unit);
        p.dry_run = true;
        let outcome = converge(&mut p, (300, 300), CONVERGE_DEADLINE, &[]);
        assert!(matches!(outcome, Err(MoveError::DryRun)), "got {outcome:?}");
    }

    #[test]
    fn bogus_low_gain_is_capped_not_flinging() {
        // A near-zero gain measurement (edge clamp, interference) would ask
        // for an i16-span correction; the step bound must keep every axis
        // within twice the remaining error plus the fixed headroom, damped
        // or not, and stay inside the wire range.
        for err in [1, 7, 100, 5_000, 100_000, i32::MAX / 4] {
            for scale in [1.0, 0.5, STEP_SCALE_MIN] {
                let step = i32::from(step_axis(err, &[], GAIN_MIN, scale));
                let bound = (2 * err.abs() + CORRECTION_HEADROOM_PX).min(i32::from(i16::MAX));
                assert!(
                    step.abs() <= bound,
                    "err {err} scale {scale}: step {step} flings past {bound}"
                );
            }
        }
        assert_eq!(step_axis(0, &[], 1.0, 1.0), 0, "no error, no step");
    }

    #[test]
    fn exhausted_deadline_stalls() {
        let mut p = FakePointer::at((0, 0), unit);
        let outcome = converge(&mut p, (300, 300), Duration::ZERO, &[]);
        assert!(matches!(outcome, Err(MoveError::Stalled { .. })));
    }

    #[test]
    fn scale_axis_rounds_and_clamps_into_i16() {
        assert_eq!(scale_axis(100, 2.0), 50);
        assert_eq!(scale_axis(-5, 0.5), -10);
        assert_eq!(scale_axis(1, 3.0), 0, "sub-half corrections round to zero");
        assert_eq!(scale_axis(99_999, 1.0), i16::MAX);
        assert_eq!(scale_axis(-99_999, 1.0), i16::MIN);
    }
}
