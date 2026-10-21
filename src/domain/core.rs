//! Poll-loop state machine (§5.3). Pure domain: depends only on the ports
//! plus an injected clock, so every transition is unit-testable with fakes.
//!
//! ```text
//! Idle -- strobe rising edge (black->white) --> Debounce
//! Debounce -- same CMD twice, >=debounce_ms apart --> Execute -> WaitClear
//!          \- strobe falls black -> Idle (edge collapsed)
//! WaitClear -- strobe back to black -> Idle
//! Watchdogs: strobe white > watchdog_ms -> log, Idle, never re-actuate.
//! Kill hotkey -> Exit (checked first, every poll).
//! ```

use std::collections::VecDeque;

use tracing::{
    debug,
    info,
    warn,
};

use crate::{
    domain::{
        Command,
        Rgb,
    },
    ports::{
        kbm::Key,
        ControlInput,
        Kbm,
        SemaphoreReader,
    },
};

/// Millisecond clock consumed by [`Core`]; injected so tests control time.
pub trait Clock {
    /// Wall time in milliseconds; only differences are meaningful.
    fn now_ms(&self) -> u64;
}

/// Fixed action coordinates resolved from config (client-relative px).
#[derive(Clone, Copy, Debug)]
pub struct CoreTargets {
    /// Native `CommoditiesBuyFrame` `BuyButton` position.
    pub buyout: (i32, i32),
    /// `AuctionHouseFrame.BuyDialog` `BuyNowButton` position.
    pub confirm: (i32, i32),
}

/// Contract timings (debounce, watchdog) plus the burst guard, config-driven.
#[derive(Clone, Copy, Debug)]
pub struct CoreTiming {
    /// The CMD color must hold on two polls at least this far apart.
    pub debounce_ms: u64,
    /// Strobe white longer than this -> treat as idle, never re-actuate.
    pub watchdog_ms: u64,
    /// Burst guard: minimum gap between two actuations (0 disables). The
    /// addon's hand-off cycle is seconds long - faster edges are a runaway
    /// signal source, and the second click is refused.
    pub min_actuation_interval_ms: u64,
    /// Burst guard: rolling per-minute actuation ceiling (>= 1).
    pub max_actuations_per_min: u32,
}

/// Lifetime counters for the heartbeat log (soak-session visibility).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CoreStats {
    /// Actuations performed (presses + ESC keys).
    pub actuations: u64,
    /// ABORT edges served (ESC sent for the error/abort path).
    pub aborts: u64,
    /// Watchdog releases (strobe stuck white / unstable CMD past the limit).
    pub watchdogs: u64,
}

/// What the caller should do after one [`Core::step`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepOutcome {
    /// Keep polling.
    Continue,
    /// Kill hotkey pressed: exit the process immediately (§3.6).
    Exit,
}

enum State {
    /// Waiting for a strobe rising edge.
    Idle,
    /// CMD observed at the edge; sampling until it proves stable.
    Debounce { cmd: Rgb, first_ms: u64, edge_ms: u64 },
    /// Actuated; waiting for the addon to rest strobe back to black.
    WaitClear { since_ms: u64 },
}

/// The core machine. Generic over its four ports, which it owns.
pub struct Core<R, K, C, T> {
    reader: R,
    kbm: K,
    kill: C,
    clock: T,
    targets: CoreTargets,
    abort_key: Key,
    timing: CoreTiming,
    state: State,
    /// Last strobe sample; `None` until the first poll so a strobe that is
    /// already white at startup is never mistaken for a fresh edge.
    last_strobe: Option<bool>,
    /// Burst guard: when the last accepted actuation happened.
    last_actuation_ms: Option<u64>,
    /// Burst guard: accepted actuation timestamps inside the last minute.
    recent_actuations: VecDeque<u64>,
    /// Burst guard warn throttle: when the last skipped-actuation warn fired
    /// (`None` = never, so the first skip warns immediately instead of
    /// being swallowed by the 2 s window).
    last_guard_warn_ms: Option<u64>,
    /// Lifetime counters (heartbeat).
    stats: CoreStats,
}

impl<R: SemaphoreReader, K: Kbm, C: ControlInput, T: Clock> Core<R, K, C, T> {
    /// Wires the core to its ports, action coordinates, abort key and timing.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        reader: R,
        kbm: K,
        kill: C,
        clock: T,
        targets: CoreTargets,
        abort_key: Key,
        timing: CoreTiming,
    ) -> Self {
        Self {
            reader,
            kbm,
            kill,
            clock,
            targets,
            abort_key,
            timing,
            state: State::Idle,
            last_strobe: None,
            last_actuation_ms: None,
            recent_actuations: VecDeque::new(),
            last_guard_warn_ms: None,
            stats: CoreStats::default(),
        }
    }

    /// Lifetime counters (heartbeat logging).
    pub fn stats(&self) -> CoreStats {
        self.stats
    }

    /// Runs one poll. Never panics on bad input: unknown colors are logged
    /// and ignored (§5.3 "never panic in the loop" holds by construction).
    pub fn step(&mut self) -> StepOutcome {
        if self.kill.kill_requested() {
            info!("kill hotkey down -> exit");
            return StepOutcome::Exit;
        }
        let now = self.clock.now_ms();
        let white = self.reader.read_strobe() == Rgb::WHITE;
        let edge = white && matches!(self.last_strobe, Some(false));

        match self.state {
            State::Idle => {
                if edge {
                    let cmd = self.reader.read_cmd();
                    debug!(cmd = %cmd, "strobe rising edge");
                    self.state = State::Debounce { cmd, first_ms: now, edge_ms: now };
                }
            }
            State::Debounce { cmd, first_ms, edge_ms } => {
                if !white {
                    debug!("strobe fell black during debounce -> idle");
                    self.state = State::Idle;
                } else if now.saturating_sub(edge_ms) > self.timing.watchdog_ms {
                    warn!("strobe white past watchdog while CMD unstable -> idle");
                    self.stats.watchdogs += 1;
                    self.state = State::Idle;
                } else {
                    let sample = self.reader.read_cmd();
                    if sample != cmd {
                        debug!(%sample, "CMD changed during debounce -> restart");
                        self.state = State::Debounce { cmd: sample, first_ms: now, edge_ms };
                    } else if now.saturating_sub(first_ms) >= self.timing.debounce_ms {
                        self.state = State::WaitClear { since_ms: now };
                        self.decode_and_execute(sample, now);
                    }
                }
            }
            State::WaitClear { since_ms } => {
                if !white {
                    debug!("strobe cleared -> idle");
                    self.state = State::Idle;
                } else if now.saturating_sub(since_ms) > self.timing.watchdog_ms {
                    warn!(
                        "strobe stuck white {}ms after actuation -> idle (no re-actuation)",
                        self.timing.watchdog_ms
                    );
                    self.stats.watchdogs += 1;
                    self.state = State::Idle;
                }
            }
        }

        self.last_strobe = Some(white);
        StepOutcome::Continue
    }

    /// Decodes a stable CMD and actuates once. The `WaitClear` state is
    /// already entered by the caller, so a failed decode still swallows the
    /// strobe until it rests black (one command per rising edge, §3.3 step 5).
    fn decode_and_execute(&mut self, sample: Rgb, now: u64) {
        let Some(command) = Command::from_rgb(sample) else {
            warn!(color = %sample, "committed CMD is not a known command -> ignored");
            return;
        };
        // Re-verify immediately before actuation (§3.6): one fresh read.
        let verified = self.reader.read_cmd();
        if verified != sample {
            warn!(expected = %sample, read = %verified, "CMD changed before actuation -> ignored");
            return;
        }
        // ABORT is the escape hatch and must never be throttled: a skipped
        // abort leaves the buy dialog stuck until the addon's own stage
        // timeout. Only purchases carry the burst guard.
        if !matches!(command, Command::Abort) && !self.guard_allows(now) {
            return;
        }
        self.stats.actuations += 1;
        match command {
            Command::Buyout => {
                let (x, y) = self.targets.buyout;
                info!(x, y, "BUYOUT: pressing native Buyout button");
                self.kbm.press(x, y);
            }
            Command::Confirm => {
                let (x, y) = self.targets.confirm;
                info!(x, y, "CONFIRM: pressing Buy Now");
                self.kbm.press(x, y);
            }
            Command::Abort => {
                self.stats.aborts += 1;
                info!(key = ?self.abort_key, "ABORT: one key per edge");
                self.kbm.key(self.abort_key);
            }
        }
    }

    /// Burst guard: refuses a purchase landing too soon after the
    /// previous one, or past the rolling per-minute ceiling. The strobe edge
    /// is still consumed (the caller already entered `WaitClear`) - the
    /// addon's own stage timeouts surface the skipped command. Sphinx must
    /// never answer a runaway signal rate with a runaway click rate; warnings
    /// are throttled so a signal storm logs, not floods. ABORT is exempt (it
    /// is the escape hatch, never a purchase).
    fn guard_allows(&mut self, now: u64) -> bool {
        if let Some(last) = self.last_actuation_ms {
            if now.saturating_sub(last) < self.timing.min_actuation_interval_ms {
                self.guard_warn(now);
                return false;
            }
        }
        while let Some(front) = self.recent_actuations.front() {
            if now.saturating_sub(*front) <= 60_000 {
                break;
            }
            self.recent_actuations.pop_front();
        }
        if self.recent_actuations.len() >= self.timing.max_actuations_per_min as usize {
            self.guard_warn(now);
            return false;
        }
        self.recent_actuations.push_back(now);
        self.last_actuation_ms = Some(now);
        true
    }

    fn guard_warn(&mut self, now: u64) {
        if self.last_guard_warn_ms.is_some_and(|last| now.saturating_sub(last) < 2_000) {
            return;
        }
        self.last_guard_warn_ms = Some(now);
        warn!(
            "actuation skipped by the burst guard (interval {}ms / ceiling {}/min)",
            self.timing.min_actuation_interval_ms, self.timing.max_actuations_per_min
        );
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{
            Cell,
            RefCell,
        },
        collections::VecDeque,
        rc::Rc,
    };

    use super::{
        Clock,
        Core,
        CoreTargets,
        CoreTiming,
        StepOutcome,
    };
    use crate::{
        domain::{
            Command,
            Rgb,
        },
        ports::{
            kbm::Key,
            ControlInput,
            Kbm,
            SemaphoreReader,
        },
    };

    /// `script` feeds successive `read_cmd` calls (edge read, debounce
    /// samples, pre-actuation re-verify); exhausted -> `fallback`.
    /// Interior-mutable so tests can drive it while `Core` holds `&mut`.
    struct FakeReader {
        strobe: Cell<Rgb>,
        fallback: Cell<Rgb>,
        script: RefCell<VecDeque<Rgb>>,
    }

    impl FakeReader {
        fn new(strobe: Rgb, script: Vec<Rgb>) -> Rc<Self> {
            Rc::new(Self {
                strobe: Cell::new(strobe),
                fallback: Cell::new(Rgb::BLACK),
                script: RefCell::new(VecDeque::from(script)),
            })
        }
    }

    impl SemaphoreReader for Rc<FakeReader> {
        fn read_cmd(&mut self) -> Rgb {
            self.script.borrow_mut().pop_front().unwrap_or_else(|| self.fallback.get())
        }

        fn read_strobe(&mut self) -> Rgb {
            self.strobe.get()
        }
    }

    #[derive(Default)]
    struct FakeKbm {
        presses: RefCell<Vec<(i32, i32)>>,
        keys: RefCell<Vec<Key>>,
    }

    impl Kbm for Rc<FakeKbm> {
        fn press(&mut self, x: i32, y: i32) {
            self.presses.borrow_mut().push((x, y));
        }

        fn key(&mut self, key: Key) {
            self.keys.borrow_mut().push(key);
        }

        fn cursor(&self) -> (i32, i32) {
            (0, 0)
        }
    }

    #[derive(Default)]
    struct FakeKill {
        down: Cell<bool>,
    }

    impl ControlInput for Rc<FakeKill> {
        fn kill_requested(&mut self) -> bool {
            self.down.get()
        }
    }

    struct FakeClock {
        t: Cell<u64>,
    }

    impl Clock for Rc<FakeClock> {
        fn now_ms(&self) -> u64 {
            self.t.get()
        }
    }

    const BUYOUT: (i32, i32) = (10, 20);
    const CONFIRM: (i32, i32) = (30, 40);

    fn make_core(
        reader: &Rc<FakeReader>,
        kbm: &Rc<FakeKbm>,
        kill: &Rc<FakeKill>,
        clock: &Rc<FakeClock>,
    ) -> Core<Rc<FakeReader>, Rc<FakeKbm>, Rc<FakeKill>, Rc<FakeClock>> {
        // The core owns clones of the shared handles; tests keep mutating
        // through the originals.
        Core::new(
            reader.clone(),
            kbm.clone(),
            kill.clone(),
            clock.clone(),
            CoreTargets { buyout: BUYOUT, confirm: CONFIRM },
            Key::Escape,
            // Guard off: existing tests exercise the machine, not the policy.
            CoreTiming {
                debounce_ms: 25,
                watchdog_ms: 10_000,
                min_actuation_interval_ms: 0,
                max_actuations_per_min: u32::MAX,
            },
        )
    }

    fn pressed(kbm: &Rc<FakeKbm>) -> Vec<(i32, i32)> {
        kbm.presses.borrow().clone()
    }

    fn clock() -> Rc<FakeClock> {
        Rc::new(FakeClock { t: Cell::new(0) })
    }

    #[test]
    fn buyout_fires_on_rising_edge_after_debounce() {
        let reader = FakeReader::new(Rgb::BLACK, vec![Rgb::RED, Rgb::RED, Rgb::RED]);
        let kbm = Rc::new(FakeKbm::default());
        let kill = Rc::new(FakeKill::default());
        let clock = clock();
        let mut core = make_core(&reader, &kbm, &kill, &clock);

        // First poll: strobe black, nothing to see.
        assert_eq!(core.step(), StepOutcome::Continue);
        assert_eq!(pressed(&kbm), [] as [(i32, i32); 0]);

        // Rising edge at t=50: CMD sampled once.
        reader.strobe.set(Rgb::WHITE);
        clock.t.set(50);
        assert_eq!(core.step(), StepOutcome::Continue);
        assert_eq!(pressed(&kbm), [] as [(i32, i32); 0]);

        // Second poll 50ms later: same CMD, debounce (25ms) satisfied -> fire.
        clock.t.set(100);
        assert_eq!(core.step(), StepOutcome::Continue);
        assert_eq!(pressed(&kbm), vec![BUYOUT]);
    }

    #[test]
    fn startup_white_strobe_is_not_an_edge() {
        // Strobe already white at startup: must wait for a black rest first.
        let reader = FakeReader::new(Rgb::WHITE, vec![Rgb::RED, Rgb::RED, Rgb::RED]);
        let kbm = Rc::new(FakeKbm::default());
        let kill = Rc::new(FakeKill::default());
        let clock = clock();
        let mut core = make_core(&reader, &kbm, &kill, &clock);

        clock.t.set(50);
        assert_eq!(core.step(), StepOutcome::Continue);
        assert_eq!(pressed(&kbm), [] as [(i32, i32); 0]);

        reader.strobe.set(Rgb::BLACK);
        clock.t.set(100);
        assert_eq!(core.step(), StepOutcome::Continue);

        reader.strobe.set(Rgb::WHITE);
        clock.t.set(150);
        assert_eq!(core.step(), StepOutcome::Continue);
        clock.t.set(200);
        assert_eq!(core.step(), StepOutcome::Continue);
        assert_eq!(pressed(&kbm), vec![BUYOUT]);
    }

    #[test]
    fn falling_strobe_during_debounce_cancels() {
        let reader = FakeReader::new(Rgb::BLACK, vec![Rgb::RED]);
        let kbm = Rc::new(FakeKbm::default());
        let kill = Rc::new(FakeKill::default());
        let clock = clock();
        let mut core = make_core(&reader, &kbm, &kill, &clock);

        // Seed a black observation so the first white is a real edge.
        assert_eq!(core.step(), StepOutcome::Continue);
        reader.strobe.set(Rgb::WHITE);
        clock.t.set(50);
        assert_eq!(core.step(), StepOutcome::Continue);
        reader.strobe.set(Rgb::BLACK);
        clock.t.set(100);
        assert_eq!(core.step(), StepOutcome::Continue);
        assert_eq!(pressed(&kbm), [] as [(i32, i32); 0]);
        assert_eq!(core.stats().actuations, 0, "the edge must have entered debounce");
    }

    #[test]
    fn reverify_blocks_actuation_when_cmd_changed() {
        // Edge read + debounce sample see RED; the pre-actuation re-read
        // sees GREEN -> must not fire.
        let reader = FakeReader::new(Rgb::BLACK, vec![Rgb::RED, Rgb::RED, Rgb::GREEN]);
        let kbm = Rc::new(FakeKbm::default());
        let kill = Rc::new(FakeKill::default());
        let clock = clock();
        let mut core = make_core(&reader, &kbm, &kill, &clock);

        // Seed a black observation so the first white is a real edge.
        assert_eq!(core.step(), StepOutcome::Continue);
        reader.strobe.set(Rgb::WHITE);
        clock.t.set(50);
        assert_eq!(core.step(), StepOutcome::Continue);
        clock.t.set(100);
        assert_eq!(core.step(), StepOutcome::Continue);
        assert_eq!(pressed(&kbm), [] as [(i32, i32); 0]);
        assert_eq!(core.stats().actuations, 0, "the edge must have reached re-verify");
    }

    #[test]
    fn cmd_flapping_until_watchdog_never_actuates() {
        // CMD alternates every poll: the debounce can never close, so the
        // Debounce-state watchdog must reclaim the edge with no actuation.
        let script: Vec<Rgb> =
            (0..64).map(|i| if i % 2 == 0 { Rgb::RED } else { Rgb::GREEN }).collect();
        let reader = FakeReader::new(Rgb::BLACK, script);
        let kbm = Rc::new(FakeKbm::default());
        let kill = Rc::new(FakeKill::default());
        let clock = clock();
        let mut core = Core::new(
            reader.clone(),
            kbm.clone(),
            kill.clone(),
            clock.clone(),
            CoreTargets { buyout: BUYOUT, confirm: CONFIRM },
            Key::Escape,
            CoreTiming {
                debounce_ms: 25,
                watchdog_ms: 200,
                min_actuation_interval_ms: 0,
                max_actuations_per_min: u32::MAX,
            },
        );

        // Seed a black observation so the first white is a real edge.
        assert_eq!(core.step(), StepOutcome::Continue);
        reader.strobe.set(Rgb::WHITE);
        clock.t.set(50);
        assert_eq!(core.step(), StepOutcome::Continue);
        for t in [100, 150, 200, 250, 300] {
            clock.t.set(t);
            assert_eq!(core.step(), StepOutcome::Continue);
        }
        // 300 - 50 = 250 > 200: the watchdog reclaimed the flapping edge.
        assert_eq!(core.stats().watchdogs, 1);
        assert_eq!(pressed(&kbm), [] as [(i32, i32); 0]);
        assert_eq!(*kbm.keys.borrow(), [] as [Key; 0]);
        assert_eq!(core.stats().actuations, 0);
    }

    #[test]
    fn cmd_change_then_stabilize_actuates_after_restart() {
        // The debounce restarts when the CMD changes, then the stable color
        // fires once its own debounce window closes.
        let reader = FakeReader::new(Rgb::BLACK, vec![Rgb::RED, Rgb::GREEN]);
        reader.fallback.set(Rgb::GREEN);
        let kbm = Rc::new(FakeKbm::default());
        let kill = Rc::new(FakeKill::default());
        let clock = clock();
        let mut core = make_core(&reader, &kbm, &kill, &clock);

        // Seed a black observation so the first white is a real edge.
        assert_eq!(core.step(), StepOutcome::Continue);
        reader.strobe.set(Rgb::WHITE);
        clock.t.set(50);
        assert_eq!(core.step(), StepOutcome::Continue, "edge reads RED");
        clock.t.set(100);
        assert_eq!(core.step(), StepOutcome::Continue, "sample GREEN != RED -> restart");
        clock.t.set(150);
        assert_eq!(core.step(), StepOutcome::Continue, "stable GREEN debounce -> fire");
        assert_eq!(pressed(&kbm), vec![CONFIRM]);
        assert_eq!(core.stats().actuations, 1);
    }

    #[test]
    fn confirm_and_abort_route_correctly() {
        let reader = FakeReader::new(Rgb::BLACK, vec![Rgb::GREEN, Rgb::GREEN, Rgb::GREEN]);
        let kbm = Rc::new(FakeKbm::default());
        let kill = Rc::new(FakeKill::default());
        let clock = clock();
        let mut core = make_core(&reader, &kbm, &kill, &clock);

        // Seed a black observation so the first white is a real edge.
        assert_eq!(core.step(), StepOutcome::Continue);

        reader.strobe.set(Rgb::WHITE);
        clock.t.set(50);
        assert_eq!(core.step(), StepOutcome::Continue);
        clock.t.set(100);
        assert_eq!(core.step(), StepOutcome::Continue);
        assert_eq!(pressed(&kbm), vec![CONFIRM]);

        // New hand-off: ABORT sends exactly one ESC.
        reader.strobe.set(Rgb::BLACK);
        clock.t.set(150);
        assert_eq!(core.step(), StepOutcome::Continue);
        reader.strobe.set(Rgb::WHITE);
        reader.fallback.set(Rgb::YELLOW);
        clock.t.set(200);
        assert_eq!(core.step(), StepOutcome::Continue);
        clock.t.set(250);
        assert_eq!(core.step(), StepOutcome::Continue);
        assert_eq!(pressed(&kbm), vec![CONFIRM], "abort must not press the mouse");
        assert_eq!(*kbm.keys.borrow(), vec![Key::Escape]);
    }

    #[test]
    fn watchdog_releases_stuck_strobe_without_reactuating() {
        let reader = FakeReader::new(Rgb::BLACK, vec![Rgb::RED, Rgb::RED, Rgb::RED, Rgb::RED]);
        reader.fallback.set(Rgb::RED);
        let kbm = Rc::new(FakeKbm::default());
        let kill = Rc::new(FakeKill::default());
        let clock = clock();
        let mut core = make_core(&reader, &kbm, &kill, &clock);

        // Seed a black observation so the first white is a real edge.
        assert_eq!(core.step(), StepOutcome::Continue);

        reader.strobe.set(Rgb::WHITE);
        clock.t.set(50);
        assert_eq!(core.step(), StepOutcome::Continue);
        clock.t.set(100);
        assert_eq!(core.step(), StepOutcome::Continue);
        assert_eq!(pressed(&kbm), vec![BUYOUT]);

        // Addon died mid-command: strobe stays white past the watchdog.
        clock.t.set(100 + 10_000 + 50);
        assert_eq!(core.step(), StepOutcome::Continue);
        assert_eq!(pressed(&kbm), vec![BUYOUT], "watchdog must not re-actuate");

        // Machine is idle again: a fresh edge works.
        reader.strobe.set(Rgb::BLACK);
        clock.t.set(10150 + 50);
        assert_eq!(core.step(), StepOutcome::Continue);
        reader.strobe.set(Rgb::WHITE);
        clock.t.set(10250);
        assert_eq!(core.step(), StepOutcome::Continue);
        clock.t.set(10300);
        assert_eq!(core.step(), StepOutcome::Continue);
        assert_eq!(pressed(&kbm), vec![BUYOUT, BUYOUT]);
    }

    #[test]
    fn one_command_per_rising_edge() {
        let reader = FakeReader::new(Rgb::BLACK, vec![Rgb::RED, Rgb::RED, Rgb::RED]);
        let kbm = Rc::new(FakeKbm::default());
        let kill = Rc::new(FakeKill::default());
        let clock = clock();
        let mut core = make_core(&reader, &kbm, &kill, &clock);

        // Seed a black observation so the first white is a real edge.
        assert_eq!(core.step(), StepOutcome::Continue);

        reader.strobe.set(Rgb::WHITE);
        clock.t.set(50);
        assert_eq!(core.step(), StepOutcome::Continue);
        clock.t.set(100);
        assert_eq!(core.step(), StepOutcome::Continue);
        // Strobe stays white (no black observed): further polls must not fire.
        for t in [150, 200, 250, 300, 350] {
            clock.t.set(t);
            assert_eq!(core.step(), StepOutcome::Continue);
        }
        assert_eq!(pressed(&kbm), vec![BUYOUT]);
    }

    #[test]
    fn kill_hotkey_exits_before_reading() {
        let reader = FakeReader::new(Rgb::WHITE, vec![]);
        let kbm = Rc::new(FakeKbm::default());
        let kill = Rc::new(FakeKill::default());
        kill.down.set(true);
        let clock = clock();
        let mut core = make_core(&reader, &kbm, &kill, &clock);

        assert_eq!(core.step(), StepOutcome::Exit);
        assert_eq!(pressed(&kbm), [] as [(i32, i32); 0]);
    }

    #[test]
    fn unknown_committed_color_is_ignored() {
        let reader = FakeReader::new(Rgb::BLACK, vec![Rgb::new(1, 2, 3)]);
        reader.fallback.set(Rgb::new(1, 2, 3));
        let kbm = Rc::new(FakeKbm::default());
        let kill = Rc::new(FakeKill::default());
        let clock = clock();
        let mut core = make_core(&reader, &kbm, &kill, &clock);

        // Seed a black observation so the first white is a real edge.
        assert_eq!(core.step(), StepOutcome::Continue);
        reader.strobe.set(Rgb::WHITE);
        clock.t.set(50);
        assert_eq!(core.step(), StepOutcome::Continue);
        clock.t.set(100);
        assert_eq!(core.step(), StepOutcome::Continue);
        assert_eq!(pressed(&kbm), [] as [(i32, i32); 0]);
        assert_eq!(*kbm.keys.borrow(), [] as [Key; 0]);
        assert_eq!(core.stats().actuations, 0, "the edge must have reached decode");
    }

    #[test]
    fn color_contract_is_exact() {
        assert!(matches!(Command::from_rgb(Rgb::RED), Some(Command::Buyout)));
        assert!(matches!(Command::from_rgb(Rgb::GREEN), Some(Command::Confirm)));
        assert!(matches!(Command::from_rgb(Rgb::YELLOW), Some(Command::Abort)));
        assert!(Command::from_rgb(Rgb::BLACK).is_none());
        assert!(Command::from_rgb(Rgb::new(255, 0, 1)).is_none());
    }

    /// One full hand-off from a resting strobe: seed black, white edge, and
    /// the post-debounce fire. Returns after the actuation (state `WaitClear`).
    fn run_cycle(
        reader: &Rc<FakeReader>,
        _kbm: &Rc<FakeKbm>,
        _kill: &Rc<FakeKill>,
        clock: &Rc<FakeClock>,
        core: &mut Core<Rc<FakeReader>, Rc<FakeKbm>, Rc<FakeKill>, Rc<FakeClock>>,
        t0: u64,
    ) {
        reader.strobe.set(Rgb::BLACK);
        clock.t.set(t0);
        assert_eq!(core.step(), StepOutcome::Continue);
        reader.strobe.set(Rgb::WHITE);
        clock.t.set(t0 + 50);
        assert_eq!(core.step(), StepOutcome::Continue);
        clock.t.set(t0 + 100);
        assert_eq!(core.step(), StepOutcome::Continue);
    }

    #[test]
    fn burst_guard_refuses_rapid_second_actuation() {
        let reader = FakeReader::new(Rgb::BLACK, vec![]);
        reader.fallback.set(Rgb::RED);
        let kbm = Rc::new(FakeKbm::default());
        let kill = Rc::new(FakeKill::default());
        let clock = clock();
        let mut core = Core::new(
            reader.clone(),
            kbm.clone(),
            kill.clone(),
            clock.clone(),
            CoreTargets { buyout: BUYOUT, confirm: CONFIRM },
            Key::Escape,
            CoreTiming {
                debounce_ms: 25,
                watchdog_ms: 10_000,
                min_actuation_interval_ms: 250,
                max_actuations_per_min: u32::MAX,
            },
        );

        run_cycle(&reader, &kbm, &kill, &clock, &mut core, 0);
        assert_eq!(pressed(&kbm), vec![BUYOUT]);

        // Second hand-off lands only 150ms after the first actuation:
        // consumed (WaitClear entered), but not clicked.
        run_cycle(&reader, &kbm, &kill, &clock, &mut core, 150);
        assert_eq!(pressed(&kbm), vec![BUYOUT]);

        // Third hand-off is far enough from the accepted actuation: fires.
        run_cycle(&reader, &kbm, &kill, &clock, &mut core, 400);
        assert_eq!(pressed(&kbm), vec![BUYOUT, BUYOUT]);
        assert_eq!(core.stats().actuations, 2);
    }

    #[test]
    fn burst_guard_per_minute_ceiling() {
        let reader = FakeReader::new(Rgb::BLACK, vec![]);
        reader.fallback.set(Rgb::RED);
        let kbm = Rc::new(FakeKbm::default());
        let kill = Rc::new(FakeKill::default());
        let clock = clock();
        let mut core = Core::new(
            reader.clone(),
            kbm.clone(),
            kill.clone(),
            clock.clone(),
            CoreTargets { buyout: BUYOUT, confirm: CONFIRM },
            Key::Escape,
            CoreTiming {
                debounce_ms: 25,
                watchdog_ms: 10_000,
                min_actuation_interval_ms: 0,
                max_actuations_per_min: 2,
            },
        );

        run_cycle(&reader, &kbm, &kill, &clock, &mut core, 100);
        run_cycle(&reader, &kbm, &kill, &clock, &mut core, 1_100);
        assert_eq!(pressed(&kbm), vec![BUYOUT, BUYOUT]);

        // Ceiling reached: the third cycle is consumed, not clicked.
        run_cycle(&reader, &kbm, &kill, &clock, &mut core, 2_100);
        assert_eq!(pressed(&kbm), vec![BUYOUT, BUYOUT]);

        // ~61 s later both earlier actuations have drained out of the 60 s
        // rolling window: actuation works again.
        run_cycle(&reader, &kbm, &kill, &clock, &mut core, 61_300);
        assert_eq!(pressed(&kbm), vec![BUYOUT, BUYOUT, BUYOUT]);
    }

    #[test]
    fn abort_is_not_throttled_by_the_burst_guard() {
        let reader = FakeReader::new(Rgb::BLACK, vec![Rgb::RED, Rgb::RED, Rgb::RED]);
        reader.fallback.set(Rgb::YELLOW);
        let kbm = Rc::new(FakeKbm::default());
        let kill = Rc::new(FakeKill::default());
        let clock = clock();
        let mut core = Core::new(
            reader.clone(),
            kbm.clone(),
            kill.clone(),
            clock.clone(),
            CoreTargets { buyout: BUYOUT, confirm: CONFIRM },
            Key::Escape,
            CoreTiming {
                debounce_ms: 25,
                watchdog_ms: 10_000,
                min_actuation_interval_ms: 250,
                max_actuations_per_min: u32::MAX,
            },
        );

        // Purchase at t=100...
        run_cycle(&reader, &kbm, &kill, &clock, &mut core, 0);
        assert_eq!(pressed(&kbm), vec![BUYOUT]);

        // ...ABORT lands 50ms after the purchase: inside the burst interval,
        // but the escape hatch must still fire.
        reader.strobe.set(Rgb::BLACK);
        clock.t.set(150);
        assert_eq!(core.step(), StepOutcome::Continue);
        reader.strobe.set(Rgb::WHITE);
        clock.t.set(200);
        assert_eq!(core.step(), StepOutcome::Continue);
        clock.t.set(250);
        assert_eq!(core.step(), StepOutcome::Continue);
        assert_eq!(*kbm.keys.borrow(), vec![Key::Escape]);
        assert_eq!(pressed(&kbm), vec![BUYOUT], "abort must not press the mouse");
        assert_eq!(core.stats().aborts, 1);
    }

    #[test]
    fn heartbeat_counters_track_actuations_aborts_watchdogs() {
        let reader = FakeReader::new(Rgb::BLACK, vec![Rgb::RED, Rgb::RED, Rgb::RED]);
        reader.fallback.set(Rgb::YELLOW);
        let kbm = Rc::new(FakeKbm::default());
        let kill = Rc::new(FakeKill::default());
        let clock = clock();
        let mut core = make_core(&reader, &kbm, &kill, &clock);

        // Seed a black observation so the first white is a real edge.
        assert_eq!(core.step(), StepOutcome::Continue);

        // Buyout fires (actuation 1)...
        reader.strobe.set(Rgb::WHITE);
        clock.t.set(50);
        assert_eq!(core.step(), StepOutcome::Continue);
        clock.t.set(100);
        assert_eq!(core.step(), StepOutcome::Continue);

        // ...the addon dies: strobe sticks white past the watchdog (+1)...
        clock.t.set(100 + 10_000 + 50);
        assert_eq!(core.step(), StepOutcome::Continue);

        // ...then an ABORT hand-off runs (actuation 2, abort 1).
        reader.strobe.set(Rgb::BLACK);
        clock.t.set(10_200);
        assert_eq!(core.step(), StepOutcome::Continue);
        reader.strobe.set(Rgb::WHITE);
        clock.t.set(10_250);
        assert_eq!(core.step(), StepOutcome::Continue);
        clock.t.set(10_300);
        assert_eq!(core.step(), StepOutcome::Continue);

        let stats = core.stats();
        assert_eq!(stats.actuations, 2);
        assert_eq!(stats.aborts, 1);
        assert_eq!(stats.watchdogs, 1);
    }
}
