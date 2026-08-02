#!/usr/bin/env bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
binary="$project_dir/target/release/termway"
artifact_dir="$project_dir/target/visual-regression"
temporary_dir=$(mktemp -d -t termway-visual.XXXXXXXX)
original_window_id=$(niri msg --json windows | jq -r '.[] | select(.is_focused) | .id' | head -n1)
output_name=$(niri msg --json focused-output | jq -r .name)
wayland_display=${WAYLAND_DISPLAY:-}
if [[ -z $wayland_display && -n ${NIRI_SOCKET:-} ]]; then
    socket_name=${NIRI_SOCKET##*/}
    wayland_display=$(cut -d. -f2 <<<"$socket_name")
fi
if [[ -z $wayland_display ]]; then
    echo "visual regression: cannot discover WAYLAND_DISPLAY" >&2
    exit 2
fi

for command in cargo grim jq kitty magick niri tmux; do
    if ! command -v "$command" >/dev/null; then
        echo "visual regression: missing command: $command" >&2
        exit 2
    fi
done

declare -a kitty_sockets=()
declare -a tmux_servers=()

cleanup() {
    for socket in "${kitty_sockets[@]}"; do
        kitty @ --to "unix:$socket" close-window --match all >/dev/null 2>&1 || true
    done
    for server in "${tmux_servers[@]}"; do
        tmux -L "$server" kill-server >/dev/null 2>&1 || true
    done
    if [[ -n ${original_window_id:-} ]]; then
        niri msg action focus-window --id "$original_window_id" >/dev/null 2>&1 || true
    fi
    case $temporary_dir in
        /tmp/termway-visual.*) rm -rf -- "$temporary_dir" ;;
    esac
}
trap cleanup EXIT

mkdir -p "$artifact_dir"
cargo build --release --manifest-path "$project_dir/Cargo.toml" >/dev/null

classify_frame() {
    local image=$1
    local width height x upper_y lower_y result
    width=$(magick identify -format '%w' "$image")
    height=$(magick identify -format '%h' "$image")
    x=$((width / 10))
    upper_y=$((height / 4))
    lower_y=$((height * 3 / 4))
    result=$(magick "$image" -format "%[fx:(u.p{$x,$upper_y}.r>.65&&u.p{$x,$upper_y}.g<.3&&u.p{$x,$upper_y}.b>.65&&u.p{$x,$lower_y}.r>.65&&u.p{$x,$lower_y}.g<.3&&u.p{$x,$lower_y}.b>.65)?1:((u.p{$x,$upper_y}.r>.65&&u.p{$x,$upper_y}.g<.3&&u.p{$x,$upper_y}.b<.3&&u.p{$x,$lower_y}.r>.65&&u.p{$x,$lower_y}.g<.3&&u.p{$x,$lower_y}.b<.3)?2:0)]" info:)
    case $result in
        1) echo magenta ;;
        2) echo red ;;
        *) echo invalid ;;
    esac
}

wait_for_kitty() {
    local socket=$1
    local attempt
    for attempt in $(seq 1 100); do
        if kitty @ --to "unix:$socket" ls >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.05
    done
    return 1
}

run_case() {
    local mode=$1
    local socket="$temporary_dir/$mode.sock"
    local log="$artifact_dir/$mode-kitty.log"
    local title="termway-visual-$mode"
    local tmux_server="termway-visual-$PPID-$mode"
    kitty_sockets+=("$socket")

    if [[ $mode == direct ]]; then
        WAYLAND_DISPLAY="$wayland_display" kitty --detach --start-as=fullscreen \
            --detached-log="$log" --listen-on="unix:$socket" \
            -o allow_remote_control=yes --class termway-vtest --title "$title" \
            env -u TMUX -u TMUX_PANE "$binary" graphics-fixture --segment-delay-ms 20
    else
        tmux_servers+=("$tmux_server")
        WAYLAND_DISPLAY="$wayland_display" kitty --detach --start-as=fullscreen \
            --detached-log="$log" --listen-on="unix:$socket" \
            -o allow_remote_control=yes --class termway-vtest --title "$title" \
            tmux -L "$tmux_server" -f /dev/null new-session \
            "tmux set-option -g allow-passthrough all; exec '$binary' graphics-fixture --segment-delay-ms 20"
    fi
    wait_for_kitty "$socket"
    sleep 0.7

    local before="$artifact_dir/$mode-before.png"
    XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)} \
        WAYLAND_DISPLAY="$wayland_display" grim -o "$output_name" "$before"
    if [[ $(classify_frame "$before") != magenta ]]; then
        echo "visual regression: $mode fixture did not reach its initial frame" >&2
        return 1
    fi

    kitty @ --to "unix:$socket" send-key --match all plus
    local reached_red=false
    local frame frame_number state
    for frame_number in $(seq 0 7); do
        frame=$(printf '%02d' "$frame_number")
        local screenshot="$artifact_dir/$mode-transition-$frame.png"
        XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)} \
            WAYLAND_DISPLAY="$wayland_display" grim -o "$output_name" "$screenshot"
        state=$(classify_frame "$screenshot")
        case $state in
            magenta) ;;
            red) reached_red=true ;;
            *)
                echo "visual regression: $mode exposed a non-atomic frame: $screenshot" >&2
                return 1
                ;;
        esac
    done
    if [[ $reached_red != true ]]; then
        echo "visual regression: $mode never displayed the requested atlas crop" >&2
        return 1
    fi

    magick "$before" "$artifact_dir/$mode-transition-00.png" \
        "$artifact_dir/$mode-transition-07.png" -thumbnail 512x320 \
        -gravity center -extent 512x320 +append "$artifact_dir/$mode-montage.png"
    kitty @ --to "unix:$socket" send-key --match all q >/dev/null 2>&1 || true
    sleep 0.2
    kitty @ --to "unix:$socket" close-window --match all >/dev/null 2>&1 || true
    sleep 0.2
    echo "visual regression: $mode passed"
}

run_case direct
run_case tmux
echo "visual regression: artifacts saved in $artifact_dir"
