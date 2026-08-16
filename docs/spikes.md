# Technical validation plan

Each spike must be able to fail on its own; on failure, record the conclusion
and re-evaluate the choice rather than carrying unknown risk into the main
implementation.

## Spike 0: Environment discovery (done)

Goal: discover the active niri session from an ordinary SSH login and an
existing tmux session.

Acceptance:

- find and connect to `$NIRI_SOCKET`;
- get outputs, windows, the focused window and the event stream;
- find the correct Wayland socket;
- do not hardcode a specific socket name in the environment across multiple
  SSH/tmux attach scenarios.

The implementation checks, in order, the command-line override, the current
process environment, the systemd user environment and the runtime directory.
When several active niri sessions are discovered it refuses to guess and
requires an explicit `--niri-socket`.

## Spike 1: View-only (done)

Goal: a `grim` screenshot, scaled, rendered to the current PTY as truecolor
half-blocks.

Acceptance:

- no termway client installed on the macOS side;
- the Terminal/Kitty → SSH → tmux chain displays correctly;
- 1.25 scale and terminal resize handled correctly;
- the CC Switch profile names are legible under local zoom;
- single-frame time, bytes and CPU recorded.

Current implementation:

- selects the target from niri's focused output by default, with `--output` override;
- fills in the `WAYLAND_DISPLAY` an SSH session is missing for `grim`;
- parses the P6 PPM from stdout directly, without creating a persistent screenshot file;
- scales proportionally with a triangle filter;
- each `▀` character carries the upper pixel in the foreground and the lower in the background;
- reads the terminal size automatically, with `--cols`/`--rows` support;
- supports `--zoom` and normalized `--center-x`/`--center-y` viewports;
- writes the image and metrics to stdout/stderr respectively;
- provides an alternate-screen interactive viewer with instant zoom, pan, resize and manual refresh;
- restores raw mode, cursor and line wrap on every error and exit path.

Measured 2026-08-01 on the reference setup (eDP-1, 2560×1600, scale 1.25),
release build:

- grim capture ≈ 30–32 ms;
- 115×36 cells rendered ≈ 9 ms;
- one ANSI frame ≈ 170 KB;
- in an 80×24 tmux pane the image is 73 cells wide with no horizontal wrap.

A focused-window capture path was also validated. niri's IPC window ID
corresponds to the foreign-toplevel identifier, but the current niri 26.04
does not implement the `ext-image-copy-capture` window-capture protocol that
`grim -T` needs, so the current version uses output-viewport zoom and does not
rely on unstable window absolute coordinates. Single-window capture can be
re-enabled once the compositor supports the protocol.

Measured over macOS SSH: half-block rendering at 5× and above with a suitable
viewport is legible enough to read CC Switch text. That renderer is therefore
positioned as the compatibility fallback when Kitty Graphics is unavailable:
1× for overview positioning, 5×–9× for reading and operating. Continuous
refresh and damage tracking belong to Spike 4.

## Spike 2: Terminal input (done)

Goal: parse and visualize terminal events only; do not inject into the desktop.

Acceptance:

- characters, arrow keys, combos, bracketed paste;
- SGR mouse move, press, release and wheel;
- identical behavior inside and outside tmux;
- terminal mode and cursor restored after an SSH interruption.

The viewer covers ASCII/Unicode, directional and function keys, modifiers,
resize, SGR left/right buttons and two-axis scrolling, and is exercised through
real tmux `send-keys`. The `Ctrl-\` prefix keeps the TUI control entry point in
INPUT mode.

## Spike 3: Home-side input injection (done)

The implementation switched to Wayland virtual pointer v2 and virtual keyboard
v1 directly, so no ydotool, uinput privileges or background service is needed.
Verified in practice: operating CC Switch, typing Chinese directly, left/right
clicks and two-axis scrolling.

Acceptance:

- can click a target profile in CC Switch;
- can type ASCII and paste Chinese;
- terminal-cell to 1.25-scale output mapping is correct;
- no exit path leaves a key held down.

## Spike 4: Native continuous capture (done)

Goal: replace `grim` with wlr-screencopy.

Baseline paths implemented:

- connects the Wayland socket the SSH session discovers;
- selects niri's target `wl_output`;
- captures the full output through `zwlr_screencopy_manager_v1`;
- uses a memfd-backed `wl_shm` buffer and reuses it across frames when size and format are unchanged;
- handles stride, XRGB/ARGB/XBGR/ABGR and Y-invert;
- automatically falls back to `grim` when the native backend is unavailable or fails at runtime;
- runs `copy_with_damage` on a separate background connection without blocking terminal input or immediate refresh;
- continuous frames run at up to 5 FPS; a single-slot latest-frame handoff ensures a slow terminal never queues stale frames;
- no ANSI redraw is sent when damage does not intersect the current viewport or visible pixels are unchanged.

The current niri 26.04 exposes wlr-screencopy v3 but not
ext-image-copy-capture. The first termway client bound the compatible
screencopy v1 for a guaranteed `wl_shm` format negotiation. The current
implementation has upgraded to v3: it waits for `buffer_done`, selects the SHM
buffer, and uses `copy_with_damage` to drive background continuous updates.

Measured 2026-08-01 on the same eDP-1, release build: a single-frame command
runs in 17.7–18.7 ms (including creating the Wayland connection), down from
warm grim's ≈30–32 ms. The viewer keeps reusing the connection and buffers.

Acceptance:

- reuse buffers (done);
- use damage or an equivalent mechanism to avoid pointless refreshes (implemented);
- interactive at the default 5 FPS (verified over SSH/tmux);
- no stale frame accumulation under SSH latency and bandwidth limits (latest-frame + non-blocking output implemented).

## Spike 5: Kitty Graphics (done)

Goal: automatically provide a high-resolution mode when supported.

Acceptance:

- direct transmission, no reference to remote file paths;
- capability probing on both the native SSH and tmux paths;
- seamless fallback to half-block when unsupported or on response timeout;
- no images linger after resize, pane switch, or detach/attach.

The current implementation includes a 1080p terminal-side navigation atlas,
instant source-crop zoom/pan, delayed tile refine, single-anchor tmux relative
placement, stable double-buffered tiles, bandwidth pacing and 1080p–360p
adaptive quality. The fixed 7-bit-per-channel preprocessing keeps the maximum
error at 1/255 while shrinking PNGs substantially. When Kitty is unavailable,
`auto` falls back to ANSI half-block with cell diff.

## MVP completion criteria

From a terminal on a macOS machine, SSH to the home NixOS host, enter tmux and
run termway:

1. find CC Switch through the overview and click-to-focus;
2. read the profile list;
3. switch profiles with the keyboard or terminal mouse;
4. open an app or invoke a compositor action through the config-driven action palette;
5. exit cleanly back to the shell;
6. nothing but SSH and the terminal is installed on the macOS side.
