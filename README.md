# sphinx

Companion input tool for [AuctiMogger]: watches the addon's on-screen pixel
semaphore (contract: `../PLAN.md` §3) and actuates the native auction-house
buy UI with trusted hardware-level input — the only kind this fork accepts
for purchases.

```
CMD pixel    black=idle  red=Buyout  green=Confirm  yellow=ABORT(ESC)
STROBE pixel black=idle  white=CMD is committed (rising edge = execute)
```

Safety behavior (§3.6): exact-color matching only, CMD re-verified right
before every actuation, strobe watchdog (white >10s -> idle, never
re-actuate), one actuation per rising edge, and a polled kill hotkey
(default `ctrl+alt+f12`, configurable via `kill_hotkey` in `sphinx.toml`)
that exits the process immediately.

## Setup

1. Copy `sphinx.toml.example` to `sphinx.toml` (gitignored).
2. In WoW: `/am semaphore coords`, paste the printed points into
   `[pixels]` / `[targets]`. Re-run after any window resize (window
   *movement* is fine — points resolve against the live client rect).
3. `cargo run -- doctor` — environment checks (DPI, window, screen DC,
   sampled pixels, foreground, hotkey). `--smoke` adds a pointer move
   test on request.
4. `cargo run -- calibrate` — live 1 Hz samples of all four points while
   you compare against `/am semaphore status`.
5. `cargo run -- run` — main loop. `--dry-run` logs actions without
   actuating (rehearsal with the addon's stage machine).

## Modes

| Command | Purpose |
|---|---|
| `run [--dry-run]` | poll loop: edge -> debounce -> verify -> actuate |
| `calibrate` | print sampled RGB at every configured point, 1 Hz |
| `doctor [--smoke]` | DPI / window / DC / pixel / foreground / hotkey checks |

## Layout (hexagonal)

- `src/domain/` — pure state machine + `Command` color contract; unit-tested
  with fake ports (edge detection, debounce, watchdog, kill, re-verify).
- `src/ports/` — `SemaphoreReader`, `ControlInput`, `Kbm` traits.
- `src/adapters/` — Win32 pixel reader (`GetPixel`), polled hotkey
  (`GetAsyncKeyState`, no hook), `sendinput` backend (enigo). The
  `window`-message and `serial` backends implement the same `Kbm` port
  later (PLAN §5.4/§8).

Gates: `cargo +nightly fmt --all --check`, `cargo clippy --all-targets -- -D
warnings`, `cargo test` (see `.github/workflows/ci.yml`).
