//! `vkbm` KBM backend (PLAN §11.4): the versa-kbm RP2040 dongle, spoken to
//! over its HID vendor control channel (usage page `0xFF00`). Built only
//! with `--features vkbm`; `sphinx.toml` selects it with
//! `backend.kind = "vkbm"`. The domain never sees any of this: same
//! [`Kbm`] port as the enigo backend.
//!
//! Wire contract: fixed 64-byte frames (`seq`, tag, payload, zero padding),
//! one ack per command, one resend on a lost ack. The resend is safe even
//! for actuation because the device dedups by sequence number within a
//! short window - a resent frame is answered from cache, not executed
//! twice. The codec below mirrors `vkbm-proto` v0.4 (the shared `no_std`
//! crate in the versa-kbm repo); the startup handshake pins the protocol
//! major version so firmware/codec drift fails loudly instead of clicking
//! wrong.
//!
//! Coordinates are screen pixels translated by the shared [`ActuationGuard`]
//! like every other backend; the screen point is then mapped endpoint-exact
//! onto the HID absolute range (`0..=32767`), which the OS scales across
//! the primary monitor - a single/primary-monitor setup, the same caveat
//! the sendinput backend documents for enigo's absolute moves.

use std::{
    fmt,
    time::{
        Duration,
        Instant,
        SystemTime,
        UNIX_EPOCH,
    },
};

use anyhow::{
    bail,
    Context,
};
use hidapi::{
    HidApi,
    HidDevice,
};
use tracing::{
    info,
    warn,
};
use windows::Win32::{
    Foundation::POINT,
    UI::WindowsAndMessaging::GetCursorPos,
};

use crate::{
    adapters::{
        game_window::{
            screen_size,
            GameWindow,
        },
        kbm::guard::ActuationGuard,
    },
    ports::{
        kbm::Key,
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

/// Highest coordinate of the HID absolute-mouse mapping (logical maximum).
const ABS_MAX: u16 = 32_767;

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

/// Wire codec mirroring `vkbm-proto` v0.4: the command subset sphinx sends,
/// the full reply vocabulary, and the 64-byte framing. Keep in sync with
/// the shared crate; the protocol-major handshake at init catches drift.
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
    const TAG_CLICK_ABS: u8 = 0x24;
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
        /// Moves the pointer to an absolute position.
        MoveAbs {
            /// Absolute X, `0..=32767`.
            x: u16,
            /// Absolute Y, `0..=32767`.
            y: u16,
        },
        /// Moves to an absolute position, then clicks the primary button.
        ClickAbs {
            /// Absolute X, `0..=32767`.
            x: u16,
            /// Absolute Y, `0..=32767`.
            y: u16,
        },
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
                Self::ClickAbs { x, y } => write!(f, "click-abs x={x} y={y}"),
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
            Cmd::ClickAbs { x, y } => {
                put_byte(out, 1, TAG_CLICK_ABS);
                put_le16(out, 2, x);
                put_le16(out, 4, y);
            }
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

/// versa-kbm actuator behind the same [`Kbm`] port as the other backends.
pub struct VkbmKbm {
    guard: ActuationGuard,
    client: Client,
    timeout: Duration,
    /// Dry-run state at init, so `smoke_echo` restores it faithfully.
    dry_run_at_init: bool,
}

impl VkbmKbm {
    /// Opens the vendor control channel, then handshakes: ping for liveness,
    /// info for the protocol version and the device gate states. The
    /// protocol major must match or init refuses - a silent mismatch would
    /// click wrong.
    ///
    /// # Errors
    /// Device not found or not openable, no ack to the ping/info, protocol
    /// major mismatch.
    pub fn new(
        vid: u16,
        pid: u16,
        ack_timeout_ms: u64,
        require_foreground: bool,
        focus_if_needed: bool,
        game: GameWindow,
    ) -> anyhow::Result<Self> {
        let api = HidApi::new().context("hidapi init")?;
        let device_info = find_device(&api, vid, pid).with_context(|| {
            format!("no versa-kbm vendor channel on {vid:04x}:{pid:04x} (plugged in? flashed?)")
        })?;
        let device = api.open_path(device_info.path()).context("opening the versa-kbm device")?;
        let mut client = Client::new(device);
        let timeout = Duration::from_millis(ack_timeout_ms);

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
        Ok(Self {
            guard: ActuationGuard::new(require_foreground, focus_if_needed, game),
            client,
            timeout,
            dry_run_at_init,
        })
    }

    /// Dry-run echo round-trip for `doctor --smoke`: turns the device's
    /// dry-run on (a configuration command; no gate involvement), sends one
    /// absolute move and expects [`wire::Reply::Echo`] - the full command
    /// path runs while nothing is physically actuated. Restores the prior
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
        let outcome = self.client.request(wire::Cmd::MoveAbs { x: 0, y: 0 }, self.timeout);
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
    fn press(&mut self, x: i32, y: i32) {
        let Some((sx, sy)) = self.guard.prepared_point("vkbm press", x, y) else {
            return;
        };
        // Same TOCTOU re-check as the other backends: the click must not
        // land in whatever window took focus between check and send.
        if !self.guard.still_prepared("vkbm press") {
            return;
        }
        let (w, h) = screen_size();
        let cmd = wire::Cmd::ClickAbs { x: to_abs(sx, w), y: to_abs(sy, h) };
        let outcome = self.client.request(cmd, self.timeout);
        let acked = matches!(&outcome, Ok(wire::Reply::Ok | wire::Reply::Echo));
        report_actuation("press", cmd, outcome);
        if acked {
            info!(client_x = x, client_y = y, screen_x = sx, screen_y = sy, "press sent");
        }
    }

    fn key(&mut self, key: Key) {
        if !self.guard.prepared_key("vkbm key") {
            return;
        }
        if !self.guard.still_prepared("vkbm key") {
            return;
        }
        let cmd = match key {
            Key::Escape => wire::Cmd::KeyTap { usage: USAGE_ESCAPE },
        };
        let outcome = self.client.request(cmd, self.timeout);
        report_actuation("key", cmd, outcome);
    }

    fn cursor(&self) -> (i32, i32) {
        // The device moves the real OS cursor, so reading it back is honest
        // diagnostics (unlike the serial backend, which has no read-back).
        let mut pt = POINT { x: 0, y: 0 };
        // SAFETY: plain cursor query into a local struct.
        if unsafe { GetCursorPos(&raw mut pt) }.is_ok() {
            (pt.x, pt.y)
        } else {
            (0, 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        initial_seq,
        issue,
        to_abs,
        wait_ack,
        wire,
        Client,
        ABS_MAX,
    };
    use crate::adapters::kbm::vkbm::{
        Link,
        LinkError,
        RequestError,
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
}
