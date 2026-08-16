# Architecture

## Process boundaries

```text
macOS terminal
  └─ ssh
      └─ tmux pane
          └─ termway                         ordinary user process
              ├─ stdin: key/paste/SGR mouse
              ├─ stdout: ANSI/Kitty Graphics
              ├─ $NIRI_SOCKET: state/action
              ├─ Wayland socket: screencopy
              ├─ Wayland virtual pointer/keyboard
              └─ session D-Bus: idle inhibit
```

SSH is the only remote boundary. termway listens on no TCP ports and needs no
extra service or privileged process.

## Module boundaries

```text
src/
  viewer.rs      state machine, TTY lifecycle, output scheduling, coordinate mapping
  kitty.rs       Kitty transport, tiles, atlas, tmux relative placements
  render.rs      half-block, cell diff, raster/viewport/tile
  screencopy.rs  wlr-screencopy session and buffer reuse
  capture.rs     damage watcher and native capture
  input.rs       Wayland virtual pointer/keyboard
  niri.rs        JSON IPC and output geometry
  config.rs      user graphics config, action palette, process environment
  idle.rs        ScreenSaver D-Bus inhibitor
```

Capture viewport, terminal cell, and niri output logical coordinates travel as
explicit structures; click mapping first returns to the capture source pixel,
then maps to output logical coordinates.

## Interaction design constraints

The interaction model is informed by Emacs and Vim: a persistent two-line
footer in the style of Emacs's mode line and echo area, modal
Navigation/Keyboard states, and `hjkl` panning.

termway is a short-session, use-it-and-leave-it tool. The interface uses
progressive disclosure and does not expect the user to remember a full state
machine:

- starts with no configuration; defaults to the focused output, 1080p and `Auto` quality;
- the persistent status line only answers "which output, what can I do now, what quality" — no internal coordinates or grid sizes;
- `?` is the single discovery entry point for all controls and `g` the only entry point for quality and resolution; the settings line shows the value, its meaning and the available arrow keys, so renderer parameters are never exposed as hotkeys;
- the config file reuses the UI's `quality` / `resolution` vocabulary; damage thresholds, transfer budgets and recovery times are advanced tuning and are not enabled by default in the example config;
- in-app adjustments affect only the current session, so a one-off experiment cannot silently change the persistent config; safety-relevant state (remote mouse, Keyboard mode, idle inhibit) is always visible.

## Operating modes

### View mode

Shows the target output or viewport. The current version captures the full
output through a persistent wlr-screencopy Wayland connection and reuses a
`wl_shm` buffer across frames. Zoom, pan and pane resize only cause local
redraws; press `r` to re-capture manually. When the native damage watcher is
available, clicks and keyboard input no longer trigger an extra capture.
Continuous mode runs
`copy_with_damage` on a separate Wayland connection at up to 5 FPS; the main
thread only polls a single latest-frame slot that overwrites the previous
value, so slow SSH output never queues up frames. Manual capture still uses an
independent, immediately-returning path and never waits on damage for a static
screen.

At startup the viewer resolves one target output: the command line wins over
the config file, and if neither is given, niri's focused output is used.
screencopy, the damage watcher, geometry mapping and the virtual pointer are
all bound to the same `wl_output`, so different scales and negative logical
coordinates on ordinary-orientation outputs never mix. The current lifetime
never switches outputs, handles output hot-plug, or non-`Normal` transforms;
full-desktop stitching is outside the current model.

ANSI redraw strategy:

- does not clear the screen at frame start; the new image directly overwrites the old one;
- cleans leftover cells at the end of each row, then any leftovers below the frame once the full image is drawn;
- uses DEC synchronized update (CSI 2026) to ask the terminal to commit a frame atomically;
- border keys and other stateless events do not trigger a redraw;
- consecutive cells reuse ANSI color state; damage frames only send the changed runs.

Kitty mode caches a configurable-resolution navigation atlas. While the
content is still fresh, zoom/pan only sends source-crop placements, refined
after 120 ms of idle by 128 px cell-aligned tiles. Desktop damage invalidates
the atlas; navigation then skips the stale crop and the 120 ms delay and
generates tiles directly from the latest captured frame. After 2 s of a still
screen the atlas is rebuilt at any viewport. The tmux path establishes a pane
anchor with a single Unicode placeholder and positions the atlas/tiles with
relative placements; crop switches are committed atomically inside a
synchronized update. Output is still burst-limited: a one-shot navigation
refine keeps the configured maximum resolution by default and only degrades
once a configured time window sees consecutive damage frames, scaled by the
configured bandwidth and the actual PNG size. A still-screen recovery redraw
forces the highest tier, so a frame is not immediately degraded again after
recovery. On a fresh atlas only a higher-resolution refine may overwrite it.
The atlas's 4 KiB APC chunks can interleave ordinary terminal control; while
the graphics queue is not drained, the old chunk is kept and the replacement
appended, avoiding a dangling `m=1` upload.

### Control mode

Ordinary input is only forwarded after an explicit key enters this mode, and
the control state is always visible. The fixed escape chord is always handled
by termway itself. Each Wayland key/button operation performs press/release in
one call, so a disconnect never leaves a key held down.

## tmux semantics

- termway is an ordinary foreground program; it does not modify the tmux server;
- after a resize the atlas and viewport are rebuilt;
- the half-block renderer needs no tmux passthrough;
- the Kitty renderer is only enabled after a capability probe succeeds;
- the tmux prefix is never taken over; the control-mode escape chord avoids `C-b` by default.

## Security boundaries

- listens on no public or LAN ports;
- SSH handles authentication and encryption;
- no real input device is ever read;
- logs must not record text input, pasted content, or raw key streams;
- read-only by default; only an explicit `--control` connects the virtual input protocols, and clicks still need a second `i` arm inside the TUI.
