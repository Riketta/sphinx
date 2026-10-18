//! Composition root + CLI: the only place naming concrete adapter types.
//! Everything below `domain`/`ports` works through traits.

use std::{
    path::PathBuf,
    thread,
    time::{
        Duration,
        Instant,
    },
};

use anyhow::{
    bail,
    Context,
};
use clap::{
    Parser,
    Subcommand,
};
use tracing::{
    info,
    warn,
};

mod adapters;
mod config;
mod domain;
mod ports;

#[cfg(feature = "serial")]
use crate::adapters::kbm::SerialKbm;
use crate::{
    adapters::{
        game_window::{
            ensure_dpi_awareness,
            foreground_hwnd,
            same_window,
            screen_size,
            system_dpi,
            window_process_image,
            window_title,
            GameWindow,
        },
        hotkey::Hotkey,
        kbm::SendInputKbm,
        pixel_reader::PixelReader,
    },
    config::{
        BackendKind,
        Config,
    },
    domain::{
        Clock,
        Command,
        Core,
        CoreTargets,
        CoreTiming,
        Rgb,
        StepOutcome,
    },
    ports::{
        Kbm,
        Key,
    },
};

/// Wall clock for the poll loop.
struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }
}

/// Actuator choice for `run`: the real backend or a log-only rehearsal one.
enum AnyKbm {
    /// enigo backend (live).
    Live(SendInputKbm),
    /// Rehearsal: logs what would happen; never sends input.
    Dry { require_foreground: bool, game: GameWindow },
    /// UART -> HID hardware input (feature `serial`).
    #[cfg(feature = "serial")]
    Serial(SerialKbm),
}

impl Kbm for AnyKbm {
    fn press(&mut self, x: i32, y: i32) {
        match self {
            Self::Live(kbm) => kbm.press(x, y),
            Self::Dry { require_foreground, game } => {
                dry_note(*require_foreground, game, "press");
                info!("[dry-run] would press ({x}, {y})");
            }
            #[cfg(feature = "serial")]
            Self::Serial(kbm) => kbm.press(x, y),
        }
    }

    fn key(&mut self, key: Key) {
        match self {
            Self::Live(kbm) => kbm.key(key),
            Self::Dry { require_foreground, game } => {
                dry_note(*require_foreground, game, "key");
                info!("[dry-run] would send {key:?}");
            }
            #[cfg(feature = "serial")]
            Self::Serial(kbm) => kbm.key(key),
        }
    }

    fn cursor(&self) -> (i32, i32) {
        match self {
            Self::Live(kbm) => kbm.cursor(),
            Self::Dry { .. } => (0, 0),
            #[cfg(feature = "serial")]
            Self::Serial(kbm) => kbm.cursor(),
        }
    }
}

/// Rehearsal-only note mirroring the live foreground guard's findings.
fn dry_note(require_foreground: bool, game: &mut GameWindow, action: &str) {
    let Some(info) = game.find() else {
        info!("[dry-run] WoW window not found (would skip {action})");
        return;
    };
    if require_foreground && !foreground_hwnd().is_some_and(|fg| same_window(fg, info.hwnd)) {
        info!("[dry-run] WoW not foreground (would skip {action})");
    }
}

#[derive(Parser)]
#[command(
    name = "sphinx",
    version,
    about = "Companion input tool for AuctiMogger: pixel semaphore -> trusted native presses"
)]
struct Cli {
    /// Path to the TOML config.
    #[arg(long, default_value = "sphinx.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Poll the semaphore and actuate (main mode).
    Run {
        /// Log actions instead of actuating (rehearsal).
        #[arg(long)]
        dry_run: bool,
    },
    /// Sample the configured points at 1 Hz (alignment aid).
    Calibrate,
    /// Environment checks: DPI, window, screen, pixels, foreground, hotkey.
    Doctor {
        /// Also move the pointer by 1 px and back (on-request smoke test).
        #[arg(long)]
        smoke: bool,
    },
}

fn main() -> anyhow::Result<()> {
    init_tracing();
    ensure_dpi_awareness();
    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)?;
    match cli.command {
        Cmd::Run { dry_run } => run(&cfg, dry_run),
        Cmd::Calibrate => calibrate(&cfg),
        Cmd::Doctor { smoke } => doctor(&cfg, smoke),
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn build_reader(cfg: &Config) -> PixelReader {
    PixelReader::new(
        GameWindow::new(&cfg.process_names()),
        (cfg.pixels.cmd.x, cfg.pixels.cmd.y),
        (cfg.pixels.strobe.x, cfg.pixels.strobe.y),
    )
}

fn run(cfg: &Config, dry_run: bool) -> anyhow::Result<()> {
    cfg.validate()?;
    let game = GameWindow::new(&cfg.process_names());
    let reader = build_reader(cfg);
    let kill = Hotkey::parse(&cfg.kill_hotkey)
        .map_err(anyhow::Error::msg)
        .context("kill_hotkey (emergency stop) is invalid")?;
    let kbm = if dry_run {
        AnyKbm::Dry { require_foreground: cfg.require_foreground, game }
    } else if cfg.backend.kind == BackendKind::Serial {
        #[cfg(feature = "serial")]
        {
            let serial = cfg.backend.serial.as_ref().context("backend.serial missing")?;
            AnyKbm::Serial(
                SerialKbm::new(
                    &serial.port,
                    serial.baud,
                    cfg.require_foreground,
                    cfg.focus_if_needed,
                    game.clone(),
                )
                .context("serial link init failed")?,
            )
        }
        #[cfg(not(feature = "serial"))]
        bail!(
            "backend.kind = \"serial\" needs a build with --features serial (validate should have caught this)"
        );
    } else {
        AnyKbm::Live(
            SendInputKbm::new(cfg.require_foreground, cfg.focus_if_needed, game)
                .context("enigo init failed")?,
        )
    };
    let kill_spec = kill.spec().to_string();
    let clock = SystemClock;
    let mut core = Core::new(
        reader,
        kbm,
        kill,
        clock,
        CoreTargets {
            buyout: (cfg.targets.buyout.x, cfg.targets.buyout.y),
            confirm: (cfg.targets.confirm.x, cfg.targets.confirm.y),
        },
        Key::Escape,
        CoreTiming {
            debounce_ms: cfg.debounce_ms,
            watchdog_ms: cfg.watchdog_ms,
            min_actuation_interval_ms: cfg.min_actuation_interval_ms,
            max_actuations_per_min: cfg.max_actuations_per_min,
        },
    );
    info!(
        poll_ms = cfg.poll_ms,
        kill = %kill_spec,
        debounce_ms = cfg.debounce_ms,
        watchdog_ms = cfg.watchdog_ms,
        backend = if dry_run { "dry-run" } else { "sendinput" },
        "sphinx running (Ctrl+C also exits)"
    );
    let start = Instant::now();
    let mut last_beat = start;
    loop {
        if core.step() == StepOutcome::Exit {
            // Immediate exit on the kill switch (§3.6): the addon's stage
            // timeouts clean up the game state.
            std::process::exit(0);
        }
        // Soak-test heartbeat (M4): one visibility line per 5 minutes, so a
        // quiet healthy session is distinguishable from a dead terminal.
        if last_beat.elapsed() >= Duration::from_secs(300) {
            let stats = core.stats();
            info!(
                uptime_s = start.elapsed().as_secs(),
                actuations = stats.actuations,
                aborts = stats.aborts,
                watchdogs = stats.watchdogs,
                "heartbeat"
            );
            last_beat = Instant::now();
        }
        thread::sleep(Duration::from_millis(cfg.poll_ms));
    }
}

fn calibrate(cfg: &Config) -> anyhow::Result<()> {
    cfg.validate()?;
    let mut reader = build_reader(cfg);
    info!("calibrate: sampling the four configured points at 1 Hz - Ctrl+C to stop");
    info!("compare with `/am semaphore status` (idle pixels = #000000)");
    loop {
        let cmd = reader.read_point((cfg.pixels.cmd.x, cfg.pixels.cmd.y));
        let strobe = reader.read_point((cfg.pixels.strobe.x, cfg.pixels.strobe.y));
        let buyout = reader.read_point((cfg.targets.buyout.x, cfg.targets.buyout.y));
        let confirm = reader.read_point((cfg.targets.confirm.x, cfg.targets.confirm.y));
        info!(
            cmd = %cmd,
            strobe = %strobe,
            buyout = %buyout,
            confirm = %confirm,
            "sample"
        );
        thread::sleep(Duration::from_secs(1));
    }
}

fn doctor(cfg: &Config, smoke: bool) -> anyhow::Result<()> {
    cfg.validate()?;
    info!("doctor: environment checks");
    let mut failures: usize = 0;

    info!(dpi = system_dpi(), "dpi: process is per-monitor aware (96 = 100%)");
    doctor_screen(cfg);
    failures += doctor_window(cfg);
    doctor_ui_scale(cfg);
    doctor_pixels(cfg);
    doctor_kill(cfg);
    failures += doctor_smoke(cfg, smoke)?;

    if failures > 0 {
        bail!("doctor: {failures} check(s) failed");
    }
    info!("doctor: all checks passed");
    Ok(())
}

fn doctor_screen(cfg: &Config) {
    let (w, h) = screen_size();
    match &cfg.screen {
        Some(s) if s.width == w && s.height == h => {
            info!(width = w, height = h, "screen: matches config");
        }
        Some(s) => {
            warn!(
                live_width = w,
                live_height = h,
                config_width = s.width,
                config_height = s.height,
                "screen: mismatch - re-run `/am semaphore coords` and update sphinx.toml"
            );
        }
        None => info!(width = w, height = h, "screen: no reference in config"),
    }
}

/// Returns 1 when the game window cannot be found (everything downstream
/// depends on it), 0 otherwise.
fn doctor_window(cfg: &Config) -> usize {
    let game = GameWindow::new(&cfg.process_names());
    let Some(info) = game.find() else {
        warn!(
            searched = ?cfg.process_names(),
            "window: no process matched - if the client runs under another \
             image name, add it to [backend.window] process_names in sphinx.toml"
        );
        return 1;
    };
    info!(
        pid = info.pid,
        title = %info.title,
        image = %window_process_image(info.hwnd).unwrap_or_default(),
        client_width = info.client.0,
        client_height = info.client.1,
        "window: game found"
    );

    match foreground_hwnd() {
        Some(fg) if same_window(fg, info.hwnd) => info!("foreground: the game window"),
        Some(fg) => {
            let title = window_title(fg);
            let image = window_process_image(fg).unwrap_or_default();
            info!(title = %title, image = %image, "foreground: another window");
        }
        None => info!("foreground: none"),
    }
    0
}

fn doctor_ui_scale(cfg: &Config) {
    if let Some(scale) = cfg.ui_scale {
        info!(ui_scale = scale, "ui_scale: reference value from the addon dump");
    }
}

fn doctor_pixels(cfg: &Config) {
    let mut reader = build_reader(cfg);
    let cmd = reader.read_point((cfg.pixels.cmd.x, cfg.pixels.cmd.y));
    let strobe = reader.read_point((cfg.pixels.strobe.x, cfg.pixels.strobe.y));
    info!(cmd = %cmd, strobe = %strobe, "pixels: semaphore samples");
    // The addon hides both frames while the semaphore is disabled, so sphinx
    // then reads whatever the game renders underneath - only exact contract
    // colors are meaningful, anything else must never actuate (and doesn't:
    // the core ignores unknown CMD colors and only fires on a white strobe).
    if strobe == Rgb::WHITE {
        warn!(
            "pixels: strobe is WHITE at rest - a command is committed, or the \
             points are misaligned; re-check with `/am semaphore coords`"
        );
    } else if cmd == Rgb::BLACK {
        info!("pixels: idle exact black - semaphore visible and idle, clean sampling");
    } else if Command::from_rgb(cmd).is_some() {
        info!(
            cmd = %cmd,
            "pixels: CMD armed (fine mid hand-off); expect strobe to commit shortly"
        );
    } else {
        info!(
            cmd = %cmd,
            strobe = %strobe,
            "pixels: non-contract colors - semaphore likely disabled (frames \
             hidden, game pixels underneath); enable `/am semaphore` to verify \
             exact-color sampling"
        );
    }
    let buyout = reader.read_point((cfg.targets.buyout.x, cfg.targets.buyout.y));
    let confirm = reader.read_point((cfg.targets.confirm.x, cfg.targets.confirm.y));
    info!(
        buyout = %buyout,
        confirm = %confirm,
        "targets: samples (meaningful with the AH buy screen open; use `calibrate` to align)"
    );
}

fn doctor_kill(cfg: &Config) {
    match Hotkey::parse(&cfg.kill_hotkey) {
        Ok(kill) => info!(
            kill = kill.spec(),
            "kill: hold to exit within one poll (GetAsyncKeyState, no hook)"
        ),
        Err(e) => warn!("kill: {e}"),
    }
}

/// Returns 1 when the smoke test was requested and failed.
fn doctor_smoke(cfg: &Config, smoke: bool) -> anyhow::Result<usize> {
    if !smoke {
        return Ok(0);
    }
    if cfg.backend.kind == BackendKind::Serial {
        // A serial build's smoke test is the link handshake itself: opening
        // the port pings the MCU, which must ack.
        #[cfg(feature = "serial")]
        {
            let serial = cfg.backend.serial.as_ref().context("backend.serial missing")?;
            SerialKbm::new(
                &serial.port,
                serial.baud,
                cfg.require_foreground,
                cfg.focus_if_needed,
                GameWindow::new(&cfg.process_names()),
            )
            .context("smoke: serial link ping failed")?;
            info!("smoke: serial link ping acked");
            return Ok(0);
        }
        #[cfg(not(feature = "serial"))]
        return Ok(0); // validate() already refused this combination
    }
    let mut kbm = SendInputKbm::new(false, false, GameWindow::new(&cfg.process_names()))
        .context("enigo init failed")?;
    let before = kbm.cursor();
    match kbm.smoke_move() {
        Ok(()) => info!(
            from_x = before.0,
            from_y = before.1,
            to_x = kbm.cursor().0,
            to_y = kbm.cursor().1,
            "smoke: pointer moved (+1, +1) and back"
        ),
        Err(e) => {
            warn!(error = %e, "smoke: pointer move failed");
            return Ok(1);
        }
    }
    Ok(0)
}
