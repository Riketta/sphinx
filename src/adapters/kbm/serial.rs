//! `serial` KBM backend (PLAN §8, v2 hardware input): a UART -> HID dongle /
//! MCU (CH9329-class, or a Pro Micro sketch) speaking the plan's line
//! protocol - `C <x> <y>\n` press, `K <code>\n` key, `P\n` ping, each acked
//! with `OK\n`. Built only with `--features serial`; `sphinx.toml` selects it
//! with `backend.kind = "serial"`. The domain never sees any of this: same
//! [`Kbm`] port as the enigo backend.
//!
//! Coordinates are screen pixels (the MCU moves the real OS cursor with an
//! absolute report), so the shared [`ActuationGuard`] translates client
//! points against the live game window exactly like the enigo backend does.

use std::{
    io::{
        self,
        Write,
    },
    time::{
        Duration,
        Instant,
    },
};

use anyhow::Context;
use tracing::{
    info,
    warn,
};

use crate::{
    adapters::{
        game_window::GameWindow,
        hotkey::VK_ESCAPE,
        kbm::guard::ActuationGuard,
    },
    ports::{
        kbm::Key,
        Kbm,
    },
};

/// How long one `OK` ack may take before the attempt counts as lost. The
/// actuation contract budgets <=150 ms per press; the honest worst case for
/// a round trip is the write plus this deadline plus one final
/// [`READ_TIMEOUT`] read (160 ms), so a slow ack slightly overshoots the
/// budget instead of ever blocking the loop indefinitely.
const ACK_TIMEOUT: Duration = Duration::from_millis(150);

/// Per-`read` port timeout. Bounds how long a single stalled byte can block
/// the poll loop (and with it the kill hotkey); the overall ack window is
/// [`ACK_TIMEOUT`], enforced in `read_ok`.
const READ_TIMEOUT: Duration = Duration::from_millis(10);

/// One command round-trip: send a line, wait for the `OK` ack. Abstracted so
/// the ack/retry logic is testable without hardware.
trait LineLink {
    /// Writes one protocol line (including its `\n`).
    fn send(&mut self, line: &str) -> io::Result<()>;
    /// Reads one ack line; `Ok` only for exactly `OK`.
    fn read_ok(&mut self) -> io::Result<()>;
}

/// Max accepted ack-line length (bytes before the `\n`); anything longer is
/// protocol garbage, not an ack.
const ACK_MAX_LINE: usize = 16;

/// Why an accumulated ack line was rejected.
#[derive(Debug, PartialEq, Eq)]
enum AckLineError {
    /// The line exceeded [`ACK_MAX_LINE`] bytes before its `\n`.
    TooLong,
}

/// Accumulates one ack line byte-by-byte: `\n` terminates the line, `\r`
/// is tolerated and stripped, and a line longer than [`ACK_MAX_LINE`] is
/// rejected. Pure protocol logic (no I/O), split out of
/// [`SerialLink::read_ok`] so the real parser is unit-testable.
struct AckLine {
    bytes: Vec<u8>,
}

impl AckLine {
    fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    /// Feeds one ack byte. Returns the completed line when the terminating
    /// `\n` arrived, or an error when the line ran past the length bound.
    fn push(&mut self, byte: u8) -> Result<Option<&[u8]>, AckLineError> {
        match byte {
            b'\n' => return Ok(Some(&self.bytes)),
            b'\r' => {}
            _ => {
                if self.bytes.len() >= ACK_MAX_LINE {
                    return Err(AckLineError::TooLong);
                }
                self.bytes.push(byte);
            }
        }
        Ok(None)
    }
}

/// Whether a completed ack line is the protocol's `OK` ack.
fn is_ok_line(line: &[u8]) -> bool {
    line == b"OK"
}

/// Real link over a `serialport` handle.
struct SerialLink {
    port: Box<dyn serialport::SerialPort>,
}

impl LineLink for SerialLink {
    fn send(&mut self, line: &str) -> io::Result<()> {
        // Best-effort: drop any ack still in flight from a previous command
        // so it cannot be consumed as *this* command's ack. A failure here
        // only re-exposes that ordering risk; the write below still decides
        // the command's outcome.
        let _ = self.port.clear(serialport::ClearBuffer::Input);
        self.port.write_all(line.as_bytes())?;
        self.port.flush()
    }

    fn read_ok(&mut self) -> io::Result<()> {
        // Read one full line (CRLF/LF tolerated, bounded) and require `OK`.
        // The overall deadline spans the whole line: per-byte port timeouts
        // alone would let a slow-drip device stall the poll loop for
        // bytes * port_timeout.
        let deadline = Instant::now() + ACK_TIMEOUT;
        let mut line = AckLine::new();
        let mut byte = [0u8; 1];
        loop {
            if Instant::now() > deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "serial ack deadline exceeded",
                ));
            }
            self.port.read_exact(&mut byte)?;
            let completed = match line.push(byte[0]) {
                Ok(completed) => completed,
                Err(AckLineError::TooLong) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "serial ack line too long",
                    ));
                }
            };
            if let Some(text) = completed {
                return if is_ok_line(text) {
                    Ok(())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unexpected serial ack {:?}", String::from_utf8_lossy(text)),
                    ))
                };
            }
        }
    }
}

/// Sends one command line with the retry policy: a **lost** ack (timeout)
/// on the idempotent ping is resent once, then the command is given up on.
/// Any other failure is returned without a resend - the device either
/// heard the command (resending could double it) or the link is broken in
/// a way no retry fixes.
fn issue(link: &mut dyn LineLink, cmd: &str) -> io::Result<()> {
    let err = match link.send(cmd).and_then(|()| link.read_ok()) {
        Ok(()) => {
            info!(line = cmd.trim_end(), attempt = 0, "serial command acked");
            return Ok(());
        }
        Err(e) => e,
    };
    if err.kind() != io::ErrorKind::TimedOut {
        warn!(error = %err, line = cmd.trim_end(), "serial command failed; not retrying");
        return Err(err);
    }
    warn!(error = %err, line = cmd.trim_end(), "serial command lost; retrying once");
    match link.send(cmd).and_then(|()| link.read_ok()) {
        Ok(()) => {
            info!(line = cmd.trim_end(), attempt = 1, "serial command acked");
            Ok(())
        }
        Err(e) => {
            warn!(error = %e, line = cmd.trim_end(), "serial command failed twice; skipping");
            Err(e)
        }
    }
}

/// Sends one actuation command **exactly once**. A lost ack means the MCU
/// may already have executed the click - resending could double it, so the
/// command is skipped and the addon's own stage timeouts surface the gap.
fn issue_once(link: &mut dyn LineLink, cmd: &str) -> io::Result<()> {
    match link.send(cmd).and_then(|()| link.read_ok()) {
        Ok(()) => {
            info!(line = cmd.trim_end(), "serial command acked");
            Ok(())
        }
        Err(e) => {
            warn!(
                error = %e,
                line = cmd.trim_end(),
                "serial command lost once; skipping (no retry for actuation)"
            );
            Err(e)
        }
    }
}

/// UART-actuator behind the same [`Kbm`] port as the enigo backend.
pub struct SerialKbm {
    guard: ActuationGuard,
    link: SerialLink,
}

impl SerialKbm {
    /// Opens the port, then pings: the MCU must ack `P` before sphinx trusts
    /// it with clicks.
    ///
    /// # Errors
    /// Port open failure, or the ping not acking.
    pub fn new(
        port_name: &str,
        baud: u32,
        require_foreground: bool,
        focus_if_needed: bool,
        game: GameWindow,
    ) -> anyhow::Result<Self> {
        let port = serialport::new(port_name, baud)
            .timeout(READ_TIMEOUT)
            .open()
            .with_context(|| format!("opening serial port {port_name} @ {baud}"))?;
        let mut link = SerialLink { port };
        issue(&mut link, "P\n")
            .with_context(|| format!("serial MCU on {port_name} did not ack the ping"))?;
        info!(port = port_name, baud, "serial link up (ping acked)");
        Ok(Self { guard: ActuationGuard::new(require_foreground, focus_if_needed, game), link })
    }
}

impl Kbm for SerialKbm {
    fn press(&mut self, x: i32, y: i32) {
        let Some((sx, sy)) = self.guard.prepared_point("serial press", x, y) else {
            return;
        };
        // Same TOCTOU re-check as the sendinput backend: the click must not
        // land in whatever window took focus between check and send.
        if !self.guard.still_prepared("serial press") {
            return;
        }
        let _ = issue_once(&mut self.link, &format!("C {sx} {sy}\n"));
    }

    fn key(&mut self, key: Key) {
        if !self.guard.prepared_key("serial key") {
            return;
        }
        if !self.guard.still_prepared("serial key") {
            return;
        }
        let code = match key {
            Key::Escape => i32::from(VK_ESCAPE),
        };
        let _ = issue_once(&mut self.link, &format!("K {code}\n"));
    }

    fn cursor(&self) -> (i32, i32) {
        // The MCU owns the pointer; sphinx has no read-back channel (v2).
        (0, 0)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        io,
    };

    use super::{
        is_ok_line,
        issue,
        issue_once,
        AckLine,
        AckLineError,
        LineLink,
    };

    #[derive(Default)]
    struct FakeLink {
        sent: Vec<String>,
        acks: VecDeque<io::Result<()>>,
    }

    impl LineLink for FakeLink {
        fn send(&mut self, line: &str) -> io::Result<()> {
            self.sent.push(line.to_string());
            Ok(())
        }

        fn read_ok(&mut self) -> io::Result<()> {
            self.acks.pop_front().unwrap_or_else(|| {
                Err(io::Error::new(io::ErrorKind::UnexpectedEof, "no ack queued"))
            })
        }
    }

    #[test]
    fn command_ok_on_first_attempt() {
        let mut link = FakeLink { acks: VecDeque::from([Ok(())]), ..FakeLink::default() };
        issue(&mut link, "P\n").ok();
        assert_eq!(link.sent, vec!["P\n".to_string()]);
    }

    #[test]
    fn lost_ping_ack_is_resent_once() {
        // `P` is idempotent: a lost ack is safe to retry.
        let mut link = FakeLink {
            acks: VecDeque::from([Err(io::Error::new(io::ErrorKind::TimedOut, "slow")), Ok(())]),
            ..FakeLink::default()
        };
        issue(&mut link, "P\n").ok();
        assert_eq!(link.sent, vec!["P\n".to_string(), "P\n".to_string()]);
    }

    #[test]
    fn lost_actuation_ack_is_never_resent() {
        // The MCU may already have executed the click when the ack is lost:
        // resending `C` could double-click a buy button.
        let mut link = FakeLink {
            acks: VecDeque::from([Err(io::Error::new(io::ErrorKind::TimedOut, "slow"))]),
            ..FakeLink::default()
        };
        issue_once(&mut link, "C 1011 441\n").ok();
        assert_eq!(
            link.sent,
            vec!["C 1011 441\n".to_string()],
            "exactly one send - no retry for actuation"
        );
    }

    #[test]
    fn double_failure_gives_up_after_two_attempts() {
        let timeout = || io::Error::new(io::ErrorKind::TimedOut, "slow");
        let mut link = FakeLink {
            acks: VecDeque::from([Err(timeout()), Err(timeout())]),
            ..FakeLink::default()
        };
        let result = issue(&mut link, "P\n");
        assert!(result.is_err());
        assert_eq!(link.sent.len(), 2, "exactly one resend, then give up");
    }

    #[test]
    fn non_timeout_failure_is_never_resent() {
        // A non-timeout failure means the device either heard the command
        // (resending could double it) or the link broke - no resend either
        // way. This also pins the no-unreachable-tail restructure of
        // `issue`.
        let mut link = FakeLink {
            acks: VecDeque::from([Err(io::Error::new(io::ErrorKind::InvalidData, "garbage"))]),
            ..FakeLink::default()
        };
        let result = issue(&mut link, "P\n");
        assert!(result.is_err());
        assert_eq!(link.sent.len(), 1, "exactly one send for a non-timeout failure");
    }

    /// Drives [`AckLine`] with every byte of `input` and returns the last
    /// outcome (what `read_ok` acts on at line end).
    fn accumulate(input: &[u8]) -> Result<Option<Vec<u8>>, AckLineError> {
        let mut line = AckLine::new();
        let mut outcome = Ok(None);
        for &byte in input {
            outcome = line.push(byte).map(|completed| completed.map(<[u8]>::to_vec));
            if outcome.is_err() {
                break;
            }
        }
        outcome
    }

    #[test]
    fn ok_line_is_accepted() {
        assert_eq!(accumulate(b"OK\n"), Ok(Some(b"OK".to_vec())));
        assert!(is_ok_line(b"OK"));
    }

    #[test]
    fn crlf_line_is_accepted() {
        // CR is tolerated and stripped wherever it appears.
        assert_eq!(accumulate(b"OK\r\n"), Ok(Some(b"OK".to_vec())));
        assert_eq!(accumulate(b"\rOK\n"), Ok(Some(b"OK".to_vec())));
        assert!(is_ok_line(b"OK"));
    }

    #[test]
    fn non_ok_line_is_consumed_but_rejected() {
        assert_eq!(accumulate(b"NO\n"), Ok(Some(b"NO".to_vec())));
        assert!(!is_ok_line(b"NO"));
        assert!(!is_ok_line(b""), "a bare newline is not an ack either");
    }

    #[test]
    fn overlong_line_errors_without_panicking() {
        // 16 bytes fit; the 17th refuses the line. `read_ok` surfaces that
        // error immediately (each command parses into a fresh `AckLine`),
        // so pushes after the error only need to stay panic-free.
        assert_eq!(accumulate(&[b'A'; 16]), Ok(None));
        assert_eq!(accumulate(&[b'A'; 17]), Err(AckLineError::TooLong));
        let mut line = AckLine::new();
        for byte in b"AAAAAAAAAAAAAAAA" {
            assert_eq!(line.push(*byte), Ok(None));
        }
        assert_eq!(line.push(b'B'), Err(AckLineError::TooLong));
        let _ = line.push(b'B');
        let _ = line.push(b'\n');
    }

    #[test]
    fn partial_line_completes_on_later_bytes() {
        assert_eq!(accumulate(b"O"), Ok(None));
        assert_eq!(accumulate(b"OK"), Ok(None));
        assert_eq!(accumulate(b"O"), Ok(None));
        let mut line = AckLine::new();
        assert_eq!(line.push(b'O'), Ok(None));
        assert_eq!(line.push(b'K'), Ok(None));
        assert_eq!(line.push(b'\n'), Ok(Some(b"OK".as_slice())));
    }
}
