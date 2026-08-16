# termway

View and control a remote Wayland desktop from an SSH terminal.

termway runs as an ordinary terminal program on your local machine and
operates a Linux host running NixOS + [niri](https://github.com/YaLTeR/niri).
It captures the desktop with `wlr-screencopy` and renders it over Kitty
Graphics, with a truecolor half-block ANSI fallback — so the remote side needs
nothing beyond what an SSH terminal already provides.

All screen capture and input injection happen on the remote host; the SSH PTY
is the only transport. No ports are opened, and nothing privileged runs
locally.

## Features

- Local side needs only an SSH client and a terminal emulator
- No open network ports; the SSH PTY is the only remote transport
- Runs as a normal fullscreen terminal program, including inside tmux
- `wlr-screencopy` capture with automatic `grim` fallback
- Kitty Graphics Protocol with source-crop navigation; ANSI fallback
- One viewer per output (multi-monitor = one viewer per output)
- Config-driven action palette
- Optional remote mouse and keyboard control (`--control`)

## Screenshot

Viewing and controlling a NixOS desktop over SSH, inside a tmux pane:
<img width="2168" height="1320" alt="Clipboard_Screenshot_1786869949" src="https://github.com/user-attachments/assets/1134454e-19f7-4886-b1fc-ba0ecaef6d37" />


## Requirements

- **Local**: any SSH client and terminal emulator
- **Remote**: a NixOS host running niri
- **tmux**: `set -g allow-passthrough on` for the tmux path
- Kitty 0.31+ is recommended for the tmux graphics path

## Quick start

```console
$ ssh host
$ tmux new-window -n gui termway
```

Capture one frame to the terminal:

```console
$ nix develop --command cargo run --release -- capture
```

Open the interactive viewer on the focused output:

```console
$ nix develop --command cargo run --release -- view
```

## Usage

- `capture` writes a single frame (image escape sequences) to stdout.
- `view` is the interactive viewer; it starts at 1× and pans/zooms as needed.

Essential controls (press `?` inside the viewer for everything):

| Key | Action |
| --- | --- |
| Left click | Move to the clicked position and zoom in one level |
| Scroll / arrows / `hjkl` | Pan |
| `+` / `-` | Zoom in / out |
| `0` | Back to the 1× overview |
| `g` | Display settings (quality, resolution) |
| `q` | Quit |

Add `--control` to send remote input: `t` switches to Keyboard mode, `i`
toggles mouse control, and `Ctrl-\` is the termway prefix (`Ctrl-\ x` opens the
action palette). Remote input uses niri's Wayland virtual pointer and
keyboard — no `ydotool`, `/dev/uinput`, or mouse-acceleration hacks.

## Configuration

Configuration is optional; without it `view` uses niri's focused output, 1080p
and the recommended `Auto` quality. Most setups need only:

```toml
output = "DP-1"           # omit to use niri's focused output

[graphics]
quality = "auto"          # auto (recommended) / sharp / fast
resolution = "1080p"      # or 720p, 1440p, 4k, native, or WIDTHxHEIGHT
```

The action palette is fully config-driven — every entry is an ordinary argv
command. Config defaults to `~/.config/termway/config.toml`; see
[`examples/config.toml`](examples/config.toml).

## Documentation

- [Architecture](docs/architecture.md) — process/module boundaries, rendering pipeline, tmux semantics, security
- [Technical selection](docs/technical-selection.md)
- [Testing](docs/testing.md)
- [ADRs](docs/adr/)

## License

MIT
