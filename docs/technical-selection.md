# Technical selection

Status: accepted for the first round of technical validation.

## Decisions

| Area | Choice | Not chosen |
| --- | --- | --- |
| Language | Rust stable, edition 2024 | C++, Go |
| Remote transport | stdin/stdout of the existing SSH PTY | custom TCP, WebSocket, QUIC |
| Terminal control | `crossterm`, raw ANSI where needed | full ratatui rendering model |
| Base image output | 24-bit ANSI + `▀` half-blocks | ASCII as the default mode |
| Enhanced image output | Kitty Graphics direct transmission | Sixel as a first-phase requirement |
| niri integration | JSON IPC over `$NIRI_SOCKET` | a version-bound `niri-ipc` crate |
| Capture validation | `grim` subprocess emitting PPM/PNG | implementing the full Wayland stack up front |
| Production capture | `wayland-client` + `wayland-protocols-wlr` screencopy | PipeWire/portal as niri's preferred path |
| Image scaling | `image` for MVP; `fast_image_resize` after perf validation | hand-written SIMD as early work |
| Production input | Wayland virtual pointer v2 + virtual keyboard v1 | `ydotool`, an uinput broker, reading input devices directly |
| Async model | main-thread terminal/output loop + single-slot background damage watcher | unbounded frame queues or a full async runtime |
| Build & development | Cargo + Nix flake | relying on a machine-global toolchain |

## Why Rust

The program simultaneously handles untrusted terminal input, Wayland buffers,
pixel math, Unix sockets and Linux input events. Rust reduces memory errors in
frame buffers and protocol parsing, and its Wayland and terminal libraries are
mature enough. The project ships a single Linux-side binary/service, so
cross-compiling to macOS is not a requirement.

## Why not a separate client/server network protocol

termway runs on the remote host after an SSH login. It reads keys and terminal
mouse sequences from the PTY and writes ANSI or image escape sequences back to
the same PTY. Authentication, encryption, port forwarding, keepalive and
access control remain SSH's job.

That directly satisfies the "no software on the company Mac" constraint and
avoids re-implementing an incomplete remote-access security protocol.

## Terminal rendering strategy

### Must work: truecolor half-block

The `▀` character's foreground color encodes the upper pixel and the
background the lower one, so a `C × R` terminal expresses `C × 2R` color
samples.

Pros:

- no dependency on a dedicated image protocol;
- passes through SSH and tmux;
- works in common modern macOS terminals;
- mouse coordinates map naturally onto the character grid.

The trade-off is limited resolution, which is why the first version must offer
local zoom and a "focus window" mode rather than only a thumbnail of the whole
high-resolution desktop.

### Enhanced mode: Kitty Graphics

The reference setup uses Kitty on both macOS and NixOS, so Kitty Graphics
direct transmission is the second-priority renderer. Remote usage cannot use
local files or shared-memory transport; the compressed pixel data must be
embedded in the escape sequence and the terminal's capability probed. The tmux
path needs separate validation of passthrough and image lifecycle.

Sixel can later become a third renderer; it is not a completion criterion for
the MVP.

### Why 50 Mbit/s cannot be the same as Sunshine/Moonlight

Kitty direct transport only accepts RGB/RGBA, PNG or zlib-compressed pixels; it
does not decode H.264/H.265.
[Waytermirror](https://github.com/cyber-wojtek/waytermirror)'s pixel renderer
also emits Kitty, but its network layer is actually server-side H.265 encoding
decoded by an installed local client, which then writes to the terminal.
Sunshine/Moonlight likewise rely on cross-frame video encoding and local
decoding. termway's hard constraint is zero-install on macOS, so it cannot hand
a video stream to Kitty — it can only send individual images or changed
regions over the SSH PTY. That is the root cause of the frame-rate difference
at equal bandwidth. The current choice — a terminal-side crop cache, damage
tiles, low-bit noise suppression and dynamic spatial resolution — prioritizes
desktop-operation latency and static-text legibility.

## niri integration

niri officially recommends connecting to `$NIRI_SOCKET` directly for complex
programs. The JSON IPC carries a compatibility promise, while the Rust
`niri-ipc` crate tracks niri's own version and does not follow an independent
stable semver. Therefore:

- a JSON event stream maintains output, workspace, window and focus state;
- JSON actions focus the target window;
- serde structures allow unknown fields so a new niri field cannot break parsing;
- `niri msg --json` is only a diagnostic and spike tool; the production path reads the Unix socket directly.

The reference environment runs niri 26.04 with 1.25 fractional scaling
enabled. Capture pixels, niri logical coordinates, and terminal cell
coordinates are three coordinate systems that must be modeled explicitly and
never mixed.

## Screen capture

The first round used `grim` to quickly answer three questions: whether an SSH
session can find the active Wayland display, what the screenshot latency is,
and how legible a scaled, terminal-rendered image is. Once validated, the
implementation moved to `zwlr_screencopy_manager_v1`:

- niri supports wlr-screencopy v3;
- capture works per output or region;
- damage information avoids emitting unchanged frames;
- no subprocess is spawned per frame.

The implementation holds one long-lived Wayland connection and reuses a
memfd-backed `wl_shm` buffer. The first version bound the backwards-compatible
protocol v1 to validate the format enumeration, then upgraded to v3: it waits
for `buffer_done` to complete format enumeration and runs `copy_with_damage`
on a separate background connection. Continuous results pass to the terminal
main thread through a single-slot latest-frame state, so a slow SSH link never
queues frames; an immediate refresh uses a separate `copy` path with a
bounded response time.

Window capture initially took the "focus the window, then capture its output
with a viewport/zoom" approach. Validation showed niri 26.04 does not yet
provide the `ext-image-copy-capture` protocol that `grim -T` needs, and IPC
window-position fields may be empty, so a window cannot be reliably captured or
cropped by absolute coordinates. The current implementation uses output
viewports independent of window coordinates; single-window capture can return
once the compositor supports the corresponding protocol.

## Input path

The terminal side enables:

- raw mode;
- alternate screen;
- bracketed paste;
- SGR extended mouse mode;
- focus events (when available).

The program turns characters, function keys and mouse cell coordinates into
internal events. niri currently exposes
`zwlr_virtual_pointer_manager_v1` and `zwp_virtual_keyboard_manager_v1`
directly, so the production path injects absolute positions, left/right
buttons, two-axis wheels and keyboard events through an ordinary-user Wayland
connection — no `ydotool`, uinput service, `input` group or privileged broker.
Non-ASCII input is sent as a Unicode code point through a small, on-demand XKB
keymap. Compositor-global shortcuts do not receive virtual-keyboard events;
those entry points are provided by the config-driven action palette, which
invokes ordinary commands.

## Initial Rust dependency candidates

Dependencies are added when the corresponding spike starts, to avoid locking
in early:

- `anyhow` / `thiserror`
- `clap`
- `crossterm`
- `serde` / `serde_json`
- `image`
- `wayland-client`
- `wayland-protocols-wlr`
- `zbus` (idle inhibitor)

No large video encoders, FFmpeg, PipeWire or GUI toolkit goes into the
first-phase dependency tree.

## Known high-risk areas

1. How an SSH-spawned process reliably discovers the graphical session's `NIRI_SOCKET`, `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR`.
2. Fractional scaling, output transform, and the terminal-cell to desktop-coordinate mapping.
3. tmux forwarding of Kitty Graphics and terminal-capability query responses.
4. macOS terminal key sequences cannot losslessly express every Linux keycode, especially modifier press/release state.
5. The virtual keyboard cannot trigger compositor-global shortcuts, so the action palette must provide those entry points.
6. Bandwidth and tmux CPU consumption of fullscreen high-frequency ANSI updates.
