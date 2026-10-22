# Pixel semaphore contract (v1)

The on-wire contract between AuctiMogger (the writer, inside WoW) and sphinx
(the reader, on the host). The writer paints two opaque marker squares on the
game window; the reader polls their exact colors and actuates the native
auction-house buy UI accordingly. The color set and the handshake below are
the contract — both sides must change together.

## Pixels

Two 4×4 px squares, `SetColorTexture`, alpha 1.0, `TOOLTIP` strata (drawn
above game UI), `EnableMouse(false)` (input-transparent), anchored to
`UIParent`'s TOPLEFT (offsets configurable; defaults below):

| Pixel  | Default offset from UIParent TOPLEFT | Purpose                          |
|--------|--------------------------------------|----------------------------------|
| CMD    | (8, −8)                              | command color                    |
| STROBE | (8, −16)                             | commit strobe (edge detection)   |

## Colors (exact RGB, no blending)

| Pixel  | Color     | Meaning                                              |
|--------|-----------|------------------------------------------------------|
| CMD    | `#000000` | idle / no command                                    |
| CMD    | `#FF0000` | BUYOUT — actuate the native Buyout button            |
| CMD    | `#00FF00` | CONFIRM — actuate the buy dialog's Buy Now button    |
| CMD    | `#FFFF00` | ABORT — press ESC; stop everything                   |
| CMD    | `#FF00FF` | AFK — tap the configured anti-AFK key once           |
| STROBE | `#000000` | idle                                                 |
| STROBE | `#FFFFFF` | the current CMD is committed (rising edge = execute) |

## Handshake

1. Writer: rest STROBE black, set the CMD color (same frame — the reader
   ignores CMD while STROBE is black, so both may change together).
2. Writer: set STROBE white after a ≥0.2 s black dwell (≥4 polls at 20 Hz so
   the gap cannot be missed; CMD stays settled throughout it — never white
   during the dwell).
3. Reader: sees the STROBE black→white edge, reads CMD, debounces (same CMD
   color on two consecutive polls ≥25 ms apart), executes the action.
4. Ack: the corresponding game event arrives at the writer (the quote event
   for BUYOUT, the purchase event for CONFIRM). On ack — or on the stage
   timeout — the writer signals the next command (every signal re-opens with
   the black rest) or resets STROBE black.
5. The reader ignores any new strobe until it has observed STROBE black at
   least once (one command per rising edge).

## Timing

- Reader poll period: ≤50 ms (≥20 Hz). Actuation latency budget after the
  rising edge: ≤150 ms on the direct absolute paths (the software
  `sendinput` backend, the board's absolute mode, and the UART backend's
  ~160 ms worst case); the default closed-loop **relative** path is allowed
  up to ~2 s to converge from any distance and **refuses to click** when it
  cannot verify the landing. The writer's 5 s per-stage timeout absorbs
  either. A configurable humanized layer sits between the edge and the
  actuation: a uniform reaction pause (default 15–50 ms) and click-position
  jitter (default ±3 px per axis), so repeated buys do not land on the same
  pixel at machine-constant timing. ABORT is never delayed or jittered.
- Writer per-stage ack timeout: 5 s (waiting for the game-event ack). On a
  BUYOUT-stage timeout: ABORT and re-actuate once within the same hand-off
  (the scan hold keeps the screen stable); the 10 s hand-off budget then
  releases quietly. On a CONFIRM-stage timeout: ABORT and release loudly
  (money may be in flight).
- ABORT (yellow) stays visible until the writer observes the abort took
  effect or its display timeout passes; the reader sends **one** ESC per
  ABORT edge — the writer may issue a second ABORT if the state did not
  clear. The ABORT display self-resets after 2 s (markers idle), and the
  writer's release path defers its clear so the yellow edge is never
  truncated to zero frames.
- AFK (magenta) is the optional anti-AFK idle reset: the writer signals it
  only during a live session with nothing in flight, on a randomized 5–10
  min cadence (addon-owned timing). The reader taps the configured key
  (`[afk_key]` in `sphinx.toml`; default `space` — bind it in WoW to a
  spell/buff or jump) once per rising edge, exempt from the burst guard,
  with no reaction pause or jitter (nothing is clicked). The display
  self-resets after 2 s like ABORT. The pulse resets the game's idle timer
  without touching the pointer or the purchase flow.

## Writer-side guarantees

- CMD is stable for ≥1 frame before STROBE goes white.
- STROBE never goes white twice without black in between; the black rest is
  ≥0.2 s (observable at ≥20 Hz).
- Colors are exact, alpha 1, drawn above everything (`TOOLTIP` strata).
- Pixels are only shown while the semaphore is enabled; disabled means both
  markers black and `signal()` is a no-op (the manual hand-off is the only
  path).

## Reader-side safety behavior

sphinx's implementation of its side of the contract:

- Exact-color matching only; the CMD color is re-verified immediately before
  each actuation (the post-debounce second read).
- One actuation per rising edge. A click is only sent at a position the
  actuator actually reached (with relative hardware input, a move that did
  not converge to the target is aborted, never clicked anyway).
- Burst guard: a purchase landing too soon after the previous one, or past
  the rolling per-minute ceiling, is refused (logged, edge consumed).
- Watchdog: STROBE white for >10 s (writer died mid-command) → log, treat
  as idle, never actuate on it.
- Foreground guard: the game window must be in the foreground; the check is
  repeated right before the click (TOCTOU re-check).
- Kill hotkey (default `ctrl+alt+f12`, polled via `GetAsyncKeyState`, no
  global hook): the process exits between actuations; the writer's stage
  timeouts clean up the game state.

## Coordinates

Physical pixels, top-left origin. The writer computes and prints paste-ready
values — run `/auctimogger semaphore coords` in WoW **with the AH buy screen
open** (the two button points are read from the live buttons):

```
cmd     = (x=7, y=7)
strobe  = (x=7, y=15)
buyout  = (x=304, y=434)
confirm = (x=285, y=315)
screen  = (width=1920, height=1080)
uiScale = 0.65
```

The printed points are **game client-area-relative**: the UI space is the
window's client area, whatever its size or position. The reader resolves
every configured point against the live window rect (`GetClientRect` +
`ClientToScreen`) at use time, so window movement never invalidates the
config — only a window resize does (re-run the command and update
`sphinx.toml`). The `screen` and `uiScale` lines are raw calibration
references; the conversion is
`physical = uiCoord × (physicalHeight / GetScreenHeight())`, then flip
`y → clientHeight − y` for top-left-origin capture APIs.

Preconditions for correct sampling: the game unobstructed at those points,
no HDR/display scaling between client and physical pixels, pure colors
(verify with `/auctimogger semaphore status` and sphinx's `calibrate` mode).
