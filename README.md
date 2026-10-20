# sphinx

Companion input tool for [AuctiMogger](https://github.com/Riketta/AuctiMogger):
watches the addon's on-screen pixel semaphore (contract: `../PLAN.md` §3) and
actuates the native auction-house buy UI with trusted hardware-level input —
the only kind this fork accepts for purchases.

```
CMD pixel    black=idle  red=Buyout  green=Confirm  yellow=ABORT(ESC)
STROBE pixel black=idle  white=CMD is committed (rising edge = execute)
```

Safety behavior (§3.6): exact-color matching only, CMD re-verified right
before every actuation, strobe watchdog (white >10s -> idle, never
re-actuate), one actuation per rising edge, and a polled kill hotkey
(default `ctrl+alt+f12`, configurable via `kill_hotkey` in `sphinx.toml`)
that exits the process immediately.

## Getting started (end to end)

The stack is three repos, each with deeper docs of its own:

| Part | Repo | Role |
|---|---|---|
| AuctiMogger (Lua addon) | [Riketta/AuctiMogger](https://github.com/Riketta/AuctiMogger) | finds deals, decides, signals via the pixel semaphore |
| sphinx (this tool) | [Riketta/sphinx](https://github.com/Riketta/sphinx) | watches the semaphore, drives the native buy UI |
| versa-kbm (RP2040 firmware + CLI) | [Riketta/versa-kbm](https://github.com/Riketta/versa-kbm) | the trusted hands: hardware-level keyboard/mouse |

**0. Prerequisites.** Windows with World of Warcraft (mainline family), a
Raspberry Pi Pico (RP2040) board + USB data cable, [Rust](https://rustup.rs)
(the repos pin their toolchains), and
[picotool](https://github.com/raspberrypi/picotool) on `PATH` for flashing.

**1. Flash the board** (once; details in the versa-kbm README): hold BOOTSEL
while plugging the board in, then from the versa-kbm repo:

```sh
cd crates/vkbm-fw
cargo run --release          # build + flash + boot the firmware
cargo run -p vkbm-cli --release -- info   # -> v0.6 switch=on arm=on
```

Solid blue LED = the device may actuate. A short BOOTSEL tap is the hardware
kill switch — the host cannot override it.

**2. Install the addon** (details in the AuctiMogger README): symlink or copy
the `AuctiMogger/` folder into `Interface/AddOns`, then set up your snipe list
and auto-buy in game (`/am`). When auto-buy hands a deal over, the addon
signals sphinx through the pixel semaphore — that is what this tool watches.

**3. Configure sphinx.** Copy `sphinx.toml.example` to `sphinx.toml`
(gitignored) and set the backend:

```toml
[backend]
kind = "vkbm"   # the RP2040 board; "sendinput" is the software fallback
```

In WoW, run `/am semaphore coords` and paste the printed points into
`[pixels]` / `[targets]`. Re-do this after any window *resize* (window
*movement* is fine — points resolve against the live client rect).

**4. Verify and calibrate:**

```sh
cargo run -- doctor            # env checks + board handshake + point checks
cargo run -- calibrate-move    # once: measure your pointer ballistics
```

`calibrate-move` measures how your Windows pointer settings amplify relative
moves (Enhance pointer precision, pointer speed — they bend the curve
strongly) and writes `move-profile.toml`; the closed loop then starts every
press already calibrated. Re-run it after changing pointer settings.
Optional spot checks: `cargo run -- press <x> <y>` drives one real guarded
click (focus WoW first), `cargo run -- calibrate` prints live pixel samples
to compare with `/am semaphore status`, and `cargo run -- run --dry-run`
rehearses the whole chain without actuating.

**5. Go live:**

```sh
cargo run -- run
```

Leave it watching. The addon signals a deal, sphinx drives the native Buyout
button through the board. Emergency stops: hold the kill hotkey
(default `ctrl+alt+f12`, configurable) or tap BOOTSEL on the board.

## Modes

| Command | Purpose |
|---|---|
| `run [--dry-run]` | poll loop: edge -> debounce -> verify -> actuate |
| `calibrate` | print sampled RGB at every configured point, 1 Hz |
| `calibrate-move` | sweep the OS ballistics curve -> `move-profile.toml` (vkbm) |
| `press <x> <y>` | one guarded real click at screen coordinates (bench) |
| `doctor [--smoke]` | DPI / window / pixel / foreground / hotkey / board checks |

## Layout (hexagonal)

- `src/domain/` — pure state machine + `Command` color contract; unit-tested
  with fake ports (edge detection, debounce, watchdog, kill, re-verify).
- `src/ports/` — `SemaphoreReader`, `ControlInput`, `Kbm` traits.
- `src/adapters/` — Win32 pixel reader (`GetPixel`), polled hotkey
  (`GetAsyncKeyState`, no hook), and the `Kbm` actuators: `vkbm` (the
  versa-kbm RP2040 board — default build), `sendinput` (enigo), and `serial`
  (UART → HID dongle/MCU, behind `--features serial`).

With several game clients open, pin discovery to the right window via
`[backend.window] title_filter = "<title substring>"` (case-insensitive);
without it, the largest visible client wins.

Gates: `cargo +nightly fmt --all --check`, `cargo clippy --all-targets -- -D
warnings`, `cargo test` (see `.github/workflows/ci.yml`).
