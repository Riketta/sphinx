//! Composition root + CLI: the only place naming concrete adapter types.
//! Everything below `domain`/`ports` works through traits.

#[cfg(feature = "vkbm")]
use std::path::Path;
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
use windows::{
    core::{
        w,
        PCWSTR,
    },
    Win32::{
        Foundation::{
            GetLastError,
            ERROR_ALREADY_EXISTS,
            HANDLE,
        },
        System::Threading::CreateMutexW,
    },
};

mod adapters;
mod config;
mod domain;
mod ports;

#[cfg(feature = "vkbm")]
use crate::adapters::kbm::vkbm::{
    calibrate_move,
    load_move_profile,
    save_move_profile,
};
#[cfg(feature = "serial")]
use crate::adapters::kbm::SerialKbm;
#[cfg(feature = "vkbm")]
use crate::adapters::kbm::VkbmKbm;
#[cfg(feature = "vkbm")]
use crate::config::MoveMode;
use crate::{
    adapters::{
        game_window::{
            ensure_dpi_awareness,
            foreground_hwnd,
            process_elevated,
            same_window,
            screen_size,
            self_elevated,
            system_dpi,
            window_process_image,
            window_title,
            GameWindow,
        },
        hotkey::{
            key_code,
            Hotkey,
            VK_ESCAPE,
        },
        kbm::SendInputKbm,
        pixel_reader::PixelReader,
        random::OsRandom,
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
        kbm::PressTiming,
        Kbm,
        Key,
    },
};

/// Exit code when the kill switch fires - distinct from a clean stop so a
/// supervisor can tell the two apart.
const EXIT_KILLED: i32 = 130;

/// Name of the `run`-mode single-instance mutex: two live sphinxes would
/// both actuate on the same semaphore edge.
const INSTANCE_MUTEX_NAME: PCWSTR = w!("sphinx-run-single-instance");

/// Monotonic clock for the poll loop: NTP steps or manual clock changes must
/// never stretch or shrink the debounce, watchdog and burst-guard windows.
struct MonotonicClock {
    start: Instant,
}

impl Clock for MonotonicClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
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
    /// versa-kbm RP2040 hardware input (feature `vkbm`).
    #[cfg(feature = "vkbm")]
    Vkbm(VkbmKbm),
}

impl Kbm for AnyKbm {
    fn press(&mut self, x: i32, y: i32) -> bool {
        match self {
            Self::Live(kbm) => kbm.press(x, y),
            Self::Dry { require_foreground, game } => {
                if dry_note(*require_foreground, game, "press") {
                    info!("[dry-run] would press ({x}, {y})");
                }
                false
            }
            #[cfg(feature = "serial")]
            Self::Serial(kbm) => kbm.press(x, y),
            #[cfg(feature = "vkbm")]
            Self::Vkbm(kbm) => kbm.press(x, y),
        }
    }

    fn key(&mut self, key: Key) -> bool {
        match self {
            Self::Live(kbm) => kbm.key(key),
            Self::Dry { require_foreground, game } => {
                if dry_note(*require_foreground, game, "key") {
                    info!("[dry-run] would send {key:?}");
                }
                false
            }
            #[cfg(feature = "serial")]
            Self::Serial(kbm) => kbm.key(key),
            #[cfg(feature = "vkbm")]
            Self::Vkbm(kbm) => kbm.key(key),
        }
    }

    fn cursor(&self) -> (i32, i32) {
        match self {
            Self::Live(kbm) => kbm.cursor(),
            Self::Dry { .. } => (0, 0),
            #[cfg(feature = "serial")]
            Self::Serial(kbm) => kbm.cursor(),
            #[cfg(feature = "vkbm")]
            Self::Vkbm(kbm) => kbm.cursor(),
        }
    }
}

/// Rehearsal-only note mirroring the live foreground guard's findings.
/// Returns whether the live backend would actuate, so the caller logs
/// exactly one outcome per event (skip *or* actuation, never both).
fn dry_note(require_foreground: bool, game: &mut GameWindow, action: &str) -> bool {
    let Some(info) = game.find() else {
        info!("[dry-run] WoW window not found (would skip {action})");
        return false;
    };
    if require_foreground && !foreground_hwnd().is_some_and(|fg| same_window(fg, info.hwnd)) {
        info!("[dry-run] WoW not foreground (would skip {action})");
        return false;
    }
    true
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
    /// Actuator override for this invocation (wins over the config's
    /// [backend] kind): sendinput, serial or vkbm.
    #[arg(long, global = true, value_enum)]
    backend: Option<BackendKind>,
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
    /// Sweep the OS pointer-ballistics curve with the vkbm board and write
    /// the move profile the closed loop seeds from (feature `vkbm`).
    #[cfg(feature = "vkbm")]
    CalibrateMove,
    /// Actuate one press at screen coordinates (bench: exercises the real
    /// backend's movement and click path exactly as `run` would).
    Press {
        /// Screen X.
        x: i32,
        /// Screen Y.
        y: i32,
    },
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
    let mut cfg = Config::load(&cli.config)?;
    if let Some(kind) = cli.backend {
        info!(?kind, "backend overridden from the command line");
        cfg.backend.kind = kind;
    }
    match cli.command {
        Cmd::Run { dry_run } => run(&cfg, dry_run),
        Cmd::Calibrate => calibrate(&cfg),
        #[cfg(feature = "vkbm")]
        Cmd::CalibrateMove => calibrate_move_cmd(&cfg),
        Cmd::Press { x, y } => press_once(&cfg, x, y),
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
        GameWindow::new(&cfg.process_names(), cfg.window_title_filter().as_deref()),
        (cfg.pixels.cmd.x, cfg.pixels.cmd.y),
        (cfg.pixels.strobe.x, cfg.pixels.strobe.y),
        cfg.pixels.cmd2.as_ref().map(|p| (p.x, p.y)),
    )
}

/// Creates the named single-instance mutex for a live `run` session. The
/// caller keeps the handle alive for the run duration; the OS reclaims it
/// at exit.
///
/// # Errors
/// Another sphinx instance is already running, or `CreateMutexW` failed.
fn acquire_instance_mutex() -> anyhow::Result<HANDLE> {
    // SAFETY: the name is a NUL-terminated wide literal; the returned
    // handle stays owned by the caller for the run duration.
    let handle = unsafe { CreateMutexW(None, false, INSTANCE_MUTEX_NAME) }
        .context("creating the single-instance mutex failed")?;
    // SAFETY: immediate last-error read after the CreateMutexW call - a
    // pre-existing mutex still returns a valid handle and reports this way.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        bail!("another sphinx instance is already running");
    }
    Ok(handle)
}

/// One-shot UIPI check for `run`: an elevated game silently drops input
/// from a non-elevated sphinx while every foreground check still passes.
/// Best effort - a failed token query (or a missing game window) just
/// skips the warning; `doctor` checks elevation strictly.
fn warn_once_if_elevation_mismatch(game: &GameWindow) {
    let Some(info) = game.find() else {
        return;
    };
    if process_elevated(info.pid).zip(self_elevated()) == Some((true, false)) {
        warn!(
            "the game runs elevated but sphinx does not - actuation will be silently dropped \
             by UIPI; run sphinx elevated"
        );
    }
}

/// Builds the actuator for `run`: the real backend, or the log-only
/// rehearsal one under `--dry-run`.
fn build_kbm(cfg: &Config, dry_run: bool, game: GameWindow) -> anyhow::Result<AnyKbm> {
    if dry_run {
        return Ok(AnyKbm::Dry { require_foreground: cfg.require_foreground, game });
    }
    if cfg.backend.kind == BackendKind::Serial {
        #[cfg(feature = "serial")]
        {
            let serial = cfg.backend.serial.as_ref().context("backend.serial missing")?;
            return Ok(AnyKbm::Serial(
                SerialKbm::new(
                    &serial.port,
                    serial.baud,
                    cfg.require_foreground,
                    cfg.focus_if_needed,
                    game.clone(),
                )
                .context("serial link init failed")?,
            ));
        }
        #[cfg(not(feature = "serial"))]
        bail!(
            "backend.kind = \"serial\" needs a build with --features serial (validate should have \
             caught this)"
        );
    }
    if cfg.backend.kind == BackendKind::Vkbm {
        #[cfg(feature = "vkbm")]
        {
            let vkbm = cfg.backend.vkbm.clone().unwrap_or_default();
            let move_profile = load_move_profile(Path::new(&vkbm.move_profile));
            if move_profile.is_empty() {
                info!(
                    path = %vkbm.move_profile,
                    "vkbm: no move profile - the closed loop learns each move (run `sphinx \
                     calibrate-move` to seed it)"
                );
            } else {
                info!(
                    path = %vkbm.move_profile,
                    samples = move_profile.len(),
                    "vkbm: move profile loaded"
                );
            }
            return Ok(AnyKbm::Vkbm(
                VkbmKbm::new(
                    vkbm.vid,
                    vkbm.pid,
                    vkbm.ack_timeout_ms,
                    vkbm.move_mode,
                    cfg.require_foreground,
                    cfg.focus_if_needed,
                    game.clone(),
                    move_profile,
                    press_timing(cfg),
                )
                .context("vkbm link init failed")?,
            ));
        }
        #[cfg(not(feature = "vkbm"))]
        bail!(
            "backend.kind = \"vkbm\" needs a build with --features vkbm (validate should have \
             caught this)"
        );
    }
    Ok(AnyKbm::Live(
        SendInputKbm::new(cfg.require_foreground, cfg.focus_if_needed, game, press_timing(cfg))
            .context("enigo init failed")?,
    ))
}

/// The humanized click-timing shape, straight from the config.
fn press_timing(cfg: &Config) -> PressTiming {
    PressTiming {
        hold_ms: cfg.hold_ms,
        hold_jitter_pct: cfg.hold_jitter_pct,
        settle_jitter_pct: cfg.settle_jitter_pct,
    }
}

fn run(cfg: &Config, dry_run: bool) -> anyhow::Result<()> {
    cfg.validate()?;
    // Single-instance guard: two live processes would both actuate on the
    // same semaphore edge. The dry-run backend never sends input, so two
    // rehearsals may run side by side. The handle must outlive the loop.
    let _instance_mutex = if dry_run { None } else { Some(acquire_instance_mutex()?) };
    let game = GameWindow::new(&cfg.process_names(), cfg.window_title_filter().as_deref());
    warn_once_if_elevation_mismatch(&game);
    let reader = build_reader(cfg);
    let kill = Hotkey::parse(&cfg.kill_hotkey)
        .map_err(anyhow::Error::msg)
        .context("kill_hotkey (emergency stop) is invalid")?;
    let kbm = build_kbm(cfg, dry_run, game)?;
    // validate() pinned [abort_key] to "escape"; mapping the config value
    // here keeps the config the single source of truth for what the core
    // sends.
    let abort_name = cfg.abort_key.key.to_lowercase();
    let abort_key = match key_code(&abort_name) {
        Some(vk) if vk == VK_ESCAPE => Key::Escape,
        _ => bail!("abort_key {:?} is not supported (v1: \"escape\")", cfg.abort_key.key),
    };
    // validate() already refused modifiers, ESC and unknown names here.
    let afk_key =
        key_code(&cfg.afk_key.key.to_lowercase()).map(Key::Vk).context("afk_key is invalid")?;
    let kill_spec = kill.spec().to_string();
    // Derive the label before `kbm` moves into the core.
    let backend_kind = if dry_run {
        "dry-run"
    } else {
        match &kbm {
            AnyKbm::Live(_) => "sendinput",
            #[cfg(feature = "serial")]
            AnyKbm::Serial(_) => "serial",
            #[cfg(feature = "vkbm")]
            AnyKbm::Vkbm(_) => "vkbm",
            AnyKbm::Dry { .. } => "dry-run",
        }
    };
    let clock = MonotonicClock { start: Instant::now() };
    let mut core = Core::new(
        reader,
        kbm,
        kill,
        clock,
        CoreTargets {
            buyout: (cfg.targets.buyout.x, cfg.targets.buyout.y),
            confirm: (cfg.targets.confirm.x, cfg.targets.confirm.y),
            buyout_item: cfg.targets.buyout_item.as_ref().map(|p| (p.x, p.y)),
            confirm_item: cfg.targets.confirm_item.as_ref().map(|p| (p.x, p.y)),
            search_item: cfg.targets.search_item.as_ref().map(|p| (p.x, p.y)),
            browse_row: cfg.targets.browse_row.as_ref().map(|p| (p.x, p.y)),
        },
        abort_key,
        afk_key,
        CoreTiming {
            debounce_ms: cfg.debounce_ms,
            watchdog_ms: cfg.watchdog_ms,
            min_actuation_interval_ms: cfg.min_actuation_interval_ms,
            max_actuations_per_min: cfg.max_actuations_per_min,
            jitter_px: cfg.jitter_px,
            reaction_min_ms: cfg.reaction_min_ms,
            reaction_max_ms: cfg.reaction_max_ms,
        },
        Box::new(OsRandom::from_entropy()),
    );
    info!(
        poll_ms = cfg.poll_ms,
        kill = %kill_spec,
        afk = %cfg.afk_key.key,
        debounce_ms = cfg.debounce_ms,
        watchdog_ms = cfg.watchdog_ms,
        backend = backend_kind,
        "sphinx running (Ctrl+C also exits)"
    );
    let start = Instant::now();
    let mut last_beat = start;
    loop {
        if core.step() == StepOutcome::Exit {
            // Immediate exit on the kill switch: the addon's stage
            // timeouts clean up the game state. Distinct exit code so a
            // supervisor sees the emergency stop.
            std::process::exit(EXIT_KILLED);
        }
        // Soak-test heartbeat: one visibility line per 5 minutes, so a
        // quiet healthy session is distinguishable from a dead terminal.
        if last_beat.elapsed() >= Duration::from_secs(300) {
            let stats = core.stats();
            info!(
                uptime_s = start.elapsed().as_secs(),
                actuations = stats.actuations,
                aborts = stats.aborts,
                afk_taps = stats.afk_taps,
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
    info!("compare with `/auctimogger semaphore status` (idle pixels = #000000)");
    loop {
        let cmd = reader.read_point((cfg.pixels.cmd.x, cfg.pixels.cmd.y));
        let strobe = reader.read_point((cfg.pixels.strobe.x, cfg.pixels.strobe.y));
        let buyout = reader.read_point((cfg.targets.buyout.x, cfg.targets.buyout.y));
        let confirm = reader.read_point((cfg.targets.confirm.x, cfg.targets.confirm.y));
        if let Some(p) = cfg.targets.buyout_item.as_ref() {
            let item = reader.read_point((p.x, p.y));
            info!(
                cmd = %cmd,
                strobe = %strobe,
                buyout = %buyout,
                buyout_item = %item,
                confirm = %confirm,
                "sample"
            );
        } else {
            info!(
                cmd = %cmd,
                strobe = %strobe,
                buyout = %buyout,
                confirm = %confirm,
                "sample (no buyout_item configured)"
            );
        }
        thread::sleep(Duration::from_secs(1));
    }
}

/// One bench press through the real backend: the same guarded path `run`
/// uses, driven directly instead of by a semaphore edge. The click is real.
fn press_once(cfg: &Config, x: i32, y: i32) -> anyhow::Result<()> {
    cfg.validate()?;
    // The bench click is real, and unlike `run` the bench commands normally
    // run one-shot: the mutex keeps two actuating sessions out of each
    // other's device sequence space (a colliding seq could draw a cached,
    // silent no-op reply instead of executing).
    let _instance = acquire_instance_mutex()?;
    let game = GameWindow::new(&cfg.process_names(), cfg.window_title_filter().as_deref());
    warn_once_if_elevation_mismatch(&game);
    let mut kbm = build_kbm(cfg, false, game)?;
    info!(x, y, "press: bench actuation");
    kbm.press(x, y);
    Ok(())
}

/// Sweeps the OS pointer-ballistics curve with the vkbm board and writes
/// the move profile the closed loop seeds from. Real cursor movement: run
/// it with the pointer somewhere with room on both sides (screen middle).
#[cfg(feature = "vkbm")]
fn calibrate_move_cmd(cfg: &Config) -> anyhow::Result<()> {
    cfg.validate()?;
    // The sweep wiggles the real cursor for about a second; the mutex keeps
    // a live `run` session (which owns the device's sequence space) out of
    // it.
    let _instance = acquire_instance_mutex()?;
    let vkbm = cfg.backend.vkbm.clone().unwrap_or_default();
    let samples = calibrate_move(vkbm.vid, vkbm.pid, vkbm.ack_timeout_ms)?;
    for (counts, gain) in &samples {
        info!(counts, px_per_count = gain, "sample");
    }
    let path = PathBuf::from(&vkbm.move_profile);
    save_move_profile(&path, &samples)?;
    info!(
        path = %path.display(),
        samples = samples.len(),
        "move profile saved (loaded automatically on the next vkbm start)"
    );
    Ok(())
}

fn doctor(cfg: &Config, smoke: bool) -> anyhow::Result<()> {
    cfg.validate()?;
    info!("doctor: environment checks");
    let mut failures: usize = 0;

    info!(dpi = system_dpi(), "dpi: process is per-monitor aware (96 = 100%)");
    failures += doctor_screen(cfg);
    failures += doctor_window(cfg);
    doctor_ui_scale(cfg);
    failures += doctor_pixels(cfg);
    doctor_kill(cfg);
    #[cfg(feature = "vkbm")]
    {
        failures += doctor_vkbm_placement(cfg);
    }
    #[cfg(feature = "serial")]
    {
        failures += doctor_serial_placement(cfg);
    }
    #[cfg(feature = "vkbm")]
    if cfg.backend.kind == BackendKind::Vkbm {
        // The handshake (and with it the firmware-minor capability gate for
        // the configured move mode) runs on every doctor pass, not only
        // under --smoke: a board that cannot serve the configured mode is
        // an environment failure, not a bench extra.
        failures += doctor_vkbm_link(cfg, smoke);
    }
    failures += doctor_smoke(cfg, smoke);

    if failures > 0 {
        bail!("doctor: {failures} check(s) failed");
    }
    info!("doctor: all checks passed");
    Ok(())
}

/// Returns 1 on a configured-vs-live screen mismatch: points calibrated for
/// another resolution sample garbage, so this is a failure, not a warning.
fn doctor_screen(cfg: &Config) -> usize {
    let (w, h) = screen_size();
    match &cfg.screen {
        Some(s) if s.width == w && s.height == h => {
            info!(width = w, height = h, "screen: matches config");
            0
        }
        Some(s) => {
            warn!(
                live_width = w,
                live_height = h,
                config_width = s.width,
                config_height = s.height,
                "screen: mismatch - re-run `/auctimogger semaphore coords` and update sphinx.toml"
            );
            1
        }
        None => {
            info!(width = w, height = h, "screen: no reference in config");
            0
        }
    }
}

/// Returns 1 when the game window cannot be found (everything downstream
/// depends on it), 0 otherwise.
fn doctor_window(cfg: &Config) -> usize {
    let game = GameWindow::new(&cfg.process_names(), cfg.window_title_filter().as_deref());
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

    // UIPI: an elevated game silently drops SendInput from a non-elevated
    // sphinx while every foreground check still passes - the worst kind of
    // failure, so doctor treats it as fatal. An unanswerable token query
    // fails closed: refusing to assume safe beats a silent dropper.
    match (process_elevated(info.pid), self_elevated()) {
        (Some(true), Some(false)) => {
            warn!(
                "window: the game runs elevated but sphinx does not - actuation will be silently \
                 dropped by UIPI; run sphinx elevated"
            );
            return 1;
        }
        (None, _) | (_, None) => {
            warn!(
                "window: could not determine elevation tokens - refusing to assume safe; run \
                 sphinx and the game at the same privilege level"
            );
            return 1;
        }
        _ => {}
    }

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

/// Returns 1 when a configured point falls outside the client rect (guaranteed
/// garbage sampling) or the strobe reads WHITE at rest (committed command or
/// misaligned points).
fn doctor_pixels(cfg: &Config) -> usize {
    let mut failures = 0;
    if let Some(info) =
        GameWindow::new(&cfg.process_names(), cfg.window_title_filter().as_deref()).find()
    {
        let mut points = vec![
            ("cmd", cfg.pixels.cmd.x, cfg.pixels.cmd.y),
            ("strobe", cfg.pixels.strobe.x, cfg.pixels.strobe.y),
            ("buyout", cfg.targets.buyout.x, cfg.targets.buyout.y),
            ("confirm", cfg.targets.confirm.x, cfg.targets.confirm.y),
        ];
        if let Some(p) = cfg.pixels.cmd2.as_ref() {
            points.push(("cmd2", p.x, p.y));
        }
        if let Some(p) = cfg.targets.buyout_item.as_ref() {
            points.push(("buyout_item", p.x, p.y));
        }
        if let Some(p) = cfg.targets.confirm_item.as_ref() {
            points.push(("confirm_item", p.x, p.y));
        }
        if let Some(p) = cfg.targets.search_item.as_ref() {
            points.push(("search_item", p.x, p.y));
        }
        if let Some(p) = cfg.targets.browse_row.as_ref() {
            points.push(("browse_row", p.x, p.y));
        }
        for (name, x, y) in points {
            if x < 0 || y < 0 || x >= info.client.0 || y >= info.client.1 {
                warn!(
                    point = name,
                    x,
                    y,
                    client_width = info.client.0,
                    client_height = info.client.1,
                    "pixels: point outside the client rect - fix it in sphinx.toml"
                );
                failures += 1;
            }
        }
    }
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
             points are misaligned; re-check with `/auctimogger semaphore coords`"
        );
        failures += 1;
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
             hidden, game pixels underneath); enable `/auctimogger semaphore` to verify \
             exact-color sampling"
        );
    }
    let buyout = reader.read_point((cfg.targets.buyout.x, cfg.targets.buyout.y));
    let confirm = reader.read_point((cfg.targets.confirm.x, cfg.targets.confirm.y));
    if let Some(p) = cfg.targets.buyout_item.as_ref() {
        let item = reader.read_point((p.x, p.y));
        info!(
            buyout = %buyout,
            buyout_item = %item,
            confirm = %confirm,
            "targets: samples (meaningful with the AH buy screen open; use `calibrate` to align)"
        );
    } else {
        info!(
            buyout = %buyout,
            confirm = %confirm,
            "targets: samples (meaningful with the AH buy screen open; use `calibrate` to align)"
        );
    }
    failures
}

fn doctor_kill(cfg: &Config) {
    // validate() has already refused an unparsable hotkey, so the Err arm is
    // a belt-and-suspenders guard, not a reachable path.
    match Hotkey::parse(&cfg.kill_hotkey) {
        Ok(kill) => info!(
            kill = kill.spec(),
            "kill: hold to exit - checked every poll, but an in-flight actuation (a relative \
             move: worst case ~2 s) finishes first (GetAsyncKeyState, no hook)"
        ),
        Err(e) => warn!("kill: {e}"),
    }
    info!(
        afk = %cfg.afk_key.key,
        "afk: key tapped once per AFK semaphore edge (addon-cadenced idle reset)"
    );
}

/// Returns 1 only when the game window lies outside the primary monitor
/// AND the configured move mode cannot reach it: absolute HID reports are
/// normalized to the primary display, while the closed-loop relative path
/// reaches every monitor - so off-primary placement is only a failure for
/// `move_mode = "abs"`, and informational otherwise.
#[cfg(feature = "vkbm")]
fn doctor_vkbm_placement(cfg: &Config) -> usize {
    if cfg.backend.kind != BackendKind::Vkbm {
        return 0;
    }
    let mode = cfg.backend.vkbm.as_ref().map_or_else(MoveMode::default, |v| v.move_mode);
    // A missing window is doctor_window's finding; placement needs one.
    let Some(info) =
        GameWindow::new(&cfg.process_names(), cfg.window_title_filter().as_deref()).find()
    else {
        return 0;
    };
    let Some((ox, oy)) = info.client_to_screen(0, 0) else {
        warn!("vkbm placement: could not locate the game window on the desktop");
        return 1;
    };
    let (w, h) = screen_size();
    let inside = ox >= 0 && oy >= 0 && ox + info.client.0 <= w && oy + info.client.1 <= h;
    if inside {
        info!(
            origin_x = ox,
            origin_y = oy,
            primary_width = w,
            primary_height = h,
            ?mode,
            "vkbm placement: game window on the primary monitor"
        );
        0
    } else if mode == MoveMode::Abs {
        warn!(
            origin_x = ox,
            origin_y = oy,
            client_width = info.client.0,
            client_height = info.client.1,
            primary_width = w,
            primary_height = h,
            "vkbm placement: game window outside the primary monitor - move_mode = \"abs\" \
             cannot reach it, actuation will be skipped; set move_mode = \"relative\" or make \
             the WoW display the primary display"
        );
        1
    } else {
        info!(
            origin_x = ox,
            origin_y = oy,
            client_width = info.client.0,
            client_height = info.client.1,
            primary_width = w,
            primary_height = h,
            ?mode,
            "vkbm placement: game window outside the primary monitor - the closed-loop \
             relative path reaches it"
        );
        0
    }
}

/// Returns 1 only when the game window lies outside the primary monitor.
/// Serial variant of [`doctor_vkbm_placement`]: the UART backend's absolute
/// reports have no relative fallback, so off-primary placement is always a
/// failure.
#[cfg(feature = "serial")]
fn doctor_serial_placement(cfg: &Config) -> usize {
    if cfg.backend.kind != BackendKind::Serial {
        return 0;
    }
    // A missing window is doctor_window's finding; placement needs one.
    let Some(info) =
        GameWindow::new(&cfg.process_names(), cfg.window_title_filter().as_deref()).find()
    else {
        return 0;
    };
    let Some((ox, oy)) = info.client_to_screen(0, 0) else {
        warn!("serial placement: could not locate the game window on the desktop");
        return 1;
    };
    let (w, h) = screen_size();
    let inside = ox >= 0 && oy >= 0 && ox + info.client.0 <= w && oy + info.client.1 <= h;
    if inside {
        info!(
            origin_x = ox,
            origin_y = oy,
            primary_width = w,
            primary_height = h,
            "serial placement: game window on the primary monitor"
        );
        0
    } else {
        warn!(
            origin_x = ox,
            origin_y = oy,
            client_width = info.client.0,
            client_height = info.client.1,
            primary_width = w,
            primary_height = h,
            "serial placement: game window outside the primary monitor - absolute UART reports \
             cannot reach it, actuation will be skipped; make the WoW display the primary display"
        );
        1
    }
}

/// Returns 1 when the vkbm link fails its handshake - device missing, no
/// ack, protocol mismatch, or the configured move mode asking for a
/// relative pointer on firmware older than 0.5. With `smoke`, additionally
/// runs the dry-run echo round-trip (nothing actuates).
#[cfg(feature = "vkbm")]
fn doctor_vkbm_link(cfg: &Config, smoke: bool) -> usize {
    let vkbm = cfg.backend.vkbm.clone().unwrap_or_default();
    let mut kbm = match VkbmKbm::new(
        vkbm.vid,
        vkbm.pid,
        vkbm.ack_timeout_ms,
        vkbm.move_mode,
        cfg.require_foreground,
        cfg.focus_if_needed,
        GameWindow::new(&cfg.process_names(), cfg.window_title_filter().as_deref()),
        load_move_profile(Path::new(&vkbm.move_profile)),
        press_timing(cfg),
    ) {
        Ok(kbm) => kbm,
        Err(e) => {
            warn!(error = %e, "vkbm link: handshake failed");
            return 1;
        }
    };
    if smoke {
        match kbm.smoke_echo() {
            Ok(()) => info!("smoke: vkbm link verified (handshake + dry-run echo)"),
            Err(e) => {
                warn!(error = %e, "smoke: vkbm dry-run echo failed");
                return 1;
            }
        }
    }
    0
}

/// Returns 1 when the smoke test was requested and failed.
fn doctor_smoke(cfg: &Config, smoke: bool) -> usize {
    if !smoke {
        return 0;
    }
    if cfg.backend.kind == BackendKind::Serial {
        // A serial build's smoke test is the link handshake itself: opening
        // the port pings the MCU, which must ack.
        #[cfg(feature = "serial")]
        {
            let Some(serial) = cfg.backend.serial.as_ref() else {
                warn!("smoke: backend.serial missing (validate should have refused)");
                return 1;
            };
            if let Err(e) = SerialKbm::new(
                &serial.port,
                serial.baud,
                cfg.require_foreground,
                cfg.focus_if_needed,
                GameWindow::new(&cfg.process_names(), cfg.window_title_filter().as_deref()),
            ) {
                warn!(error = %e, "smoke: serial link ping failed");
                return 1;
            }
            info!("smoke: serial link ping acked");
            return 0;
        }
        #[cfg(not(feature = "serial"))]
        return 0; // validate() already refused this combination
    }
    if cfg.backend.kind == BackendKind::Vkbm {
        // The vkbm link check (handshake + capability gate) is doctoring's
        // always-on `doctor_vkbm_link`; the dry-run echo rides `smoke`.
        return 0;
    }
    let mut kbm = match SendInputKbm::new(
        false,
        false,
        GameWindow::new(&cfg.process_names(), cfg.window_title_filter().as_deref()),
        press_timing(cfg),
    ) {
        Ok(kbm) => kbm,
        Err(e) => {
            warn!(error = %e, "smoke: enigo init failed");
            return 1;
        }
    };
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
            return 1;
        }
    }
    0
}
