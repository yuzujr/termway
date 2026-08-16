# ADR-0001: Adopt an SSH-native architecture

- Status: accepted
- Date: 2026-08-01

## Context

The user accesses a home NixOS/niri host over SSH and tmux from a managed
macOS laptop. macOS cannot install a remote-desktop client, let alone grant
high-privilege access such as reading input devices.

The previous Waytermirror approach used a separate client/server pair over raw
TCP streams. Its client read the local keyboard and mouse directly through
libinput, so it cannot be treated as reading PTY input inside a remote tmux
pane.

## Decision

termway itself runs on the remote NixOS host. It interacts with the user only
through the current PTY's stdin/stdout, defines no cross-network application
protocol, and needs no macOS binary.

Screen capture, input injection and niri state all come from the remote
graphical session. niri's Wayland virtual pointer/keyboard protocols are
sufficient for input, so the implementation needs no privileged broker.

## Consequences

Positive:

- zero-install on macOS;
- directly compatible with SSH's authentication, encryption and audit;
- runs naturally inside tmux;
- no extra open ports.

Costs:

- limited by the terminal protocol's expressive power;
- cannot get full key press/release fidelity the way a native client can;
- enhanced capabilities such as Kitty/Sixel depend on the terminal and tmux;
- audio and high-frame-rate video are not suitable targets.
