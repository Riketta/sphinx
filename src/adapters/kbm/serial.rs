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
/// actuation contract budgets <=150 ms per press, so the ack window stays
/// inside it.
const ACK_TIMEOUT: Duration = Duration::from_millis(150);

/// Per-`read` port timeout. Bounds how long a single stalled byte can block
/// the poll loop (and with it the kill hotkey); the overall ack window is
/// [`ACK_TIMEOUT`], enforced in `read_ok`.
const READ_TIMEOUT: Duration = Duration::from_millis(25);

/// One command round-trip: send a line, wait for the `OK` ack. Abstracted so
/// the ack/retry logic is testable without hardware.
trait LineLink {
    /// Writes one protocol line (including its `\n`).
    fn send(&mut self, line: &str) -> io::Result<()>;
    /// Reads one ack line; `Ok` only for exactly `OK`.
    fn read_ok(&mut self) -> io::Result<()>;
}

/// Real link over a `serialport` handle.
struct SerialLink {
    port: Box<dyn serialport::SerialPort>,
}

impl LineLink for SerialLink {
    fn send(&mut self, line: &str) -> io::Result<()> {
        self.port.write_all(line.as_bytes())?;
        self.port.flush()
    }

    fn read_ok(&mut self) -> io::Result<()> {
        // Read one full line (CRLF/LF tolerated, bounded) and require `OK`.
        // The overall deadline spans the whole line: per-byte port timeouts
        // alone would let a slow-drip device stall the poll loop for
        // bytes * port_timeout.
        let deadline = Instant::now() + ACK_TIMEOUT;
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            if Instant::now() > deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "serial ack deadline exceeded",
                ));
            }
            self.port.read_exact(&mut byte)?;
            if byte[0] == b'\n' {
                break;
            }
            if byte[0] != b'\r' {
                line.push(byte[0]);
            }
            if line.len() > 16 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "serial ack line too long"));
            }
        }
        if line == b"OK" {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected serial ack {:?}", String::from_utf8_lossy(&line)),
            ))
        }
    }
}

/// Sends one command line with the retry policy: a lost ack on the
/// **idempotent** ping is resent once, then the command is given up on.
fn issue(link: &mut dyn LineLink, cmd: &str) -> io::Result<()> {
    for attempt in 0..2 {
        match link.send(cmd).and_then(|()| link.read_ok()) {
            Ok(()) => {
                info!(line = cmd.trim_end(), attempt, "serial command acked");
                return Ok(());
            }
            Err(e) if attempt == 0 => {
                warn!(error = %e, line = cmd.trim_end(), "serial command lost; retrying once");
            }
            Err(e) => {
                warn!(error = %e, line = cmd.trim_end(), "serial command failed twice; skipping");
                return Err(e);
            }
        }
    }
    Ok(())
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
        let _ = issue_once(&mut self.link, &format!("C {sx} {sy}\n"));
    }

    fn key(&mut self, key: Key) {
        if !self.guard.prepared_key("serial key") {
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
        issue,
        issue_once,
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
}
