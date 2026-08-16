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

for command in awk cargo cmp grim jq kitty magick niri tmux; do
    if ! command -v "$command" >/dev/null; then
        echo "visual regression: missing command: $command" >&2
        exit 2
    fi
done

if command -v loginctl >/dev/null; then
    graphical_session=$(loginctl list-sessions --no-legend 2>/dev/null \
        | awk '$4 == "seat0" { print $1; exit }')
    if [[ -n $graphical_session ]] \
        && [[ $(loginctl show-session "$graphical_session" -p LockedHint --value 2>/dev/null) == yes ]]; then
        echo "visual regression: graphical session is locked; unlock it before running pixel checks" >&2
        exit 2
    fi
fi

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

run_stale_atlas_case() {
    local mode=$1
    local socket="$temporary_dir/stale-$mode.sock"
    local log="$artifact_dir/stale-$mode-kitty.log"
    local title="termway-stale-$mode"
    local tmux_server="termway-visual-$PPID-stale-$mode"
    kitty_sockets+=("$socket")

    if [[ $mode == direct ]]; then
        WAYLAND_DISPLAY="$wayland_display" kitty --detach --start-as=fullscreen \
            --detached-log="$log" --listen-on="unix:$socket" \
            -o allow_remote_control=yes --class termway-vtest --title "$title" \
            env -u TMUX -u TMUX_PANE "$binary" quality-fixture \
            --tmux-bandwidth-mbps 40 --refine-delay-ms 750 --atlas-refresh-ms 10000
    else
        tmux_servers+=("$tmux_server")
        WAYLAND_DISPLAY="$wayland_display" kitty --detach --start-as=fullscreen \
            --detached-log="$log" --listen-on="unix:$socket" \
            -o allow_remote_control=yes --class termway-vtest --title "$title" \
            tmux -L "$tmux_server" -f /dev/null new-session \
            "tmux set-option -g allow-passthrough all; exec '$binary' quality-fixture --tmux-bandwidth-mbps 40 --refine-delay-ms 750 --atlas-refresh-ms 10000"
    fi
    wait_for_kitty "$socket"

    local attempt screen_text=""
    for attempt in $(seq 1 200); do
        screen_text=$(kitty @ --to "unix:$socket" get-text --match all --extent screen 2>/dev/null || true)
        if [[ $screen_text == *'1.00×'* ]]; then
            break
        fi
        sleep 0.01
    done
    if [[ $screen_text != *'1.00×'* ]]; then
        echo "visual regression: $mode stale-atlas fixture did not initialize" >&2
        return 1
    fi
    # The modeline can arrive before the initial atlas payload and fullscreen resize settle.
    # Inject damage only after both direct and paced tmux transports have had time to finish it.
    sleep 0.7

    # Replace the detailed initial frame with magenta tiles while deliberately leaving its atlas
    # stale. The extended fixture delay gives an incorrect cached preview a stable 750ms window.
    kitty @ --to "unix:$socket" send-key --match all d
    local state=invalid current=""
    for attempt in $(seq 1 40); do
        current="$artifact_dir/stale-$mode-current.png"
        XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)} \
            WAYLAND_DISPLAY="$wayland_display" grim -o "$output_name" "$current"
        state=$(classify_frame "$current")
        if [[ $state == magenta ]]; then
            break
        fi
        sleep 0.01
    done
    if [[ $state != magenta ]]; then
        echo "visual regression: $mode stale-atlas fixture did not display its current frame" >&2
        return 1
    fi

    kitty @ --to "unix:$socket" send-key --match all plus
    screen_text=""
    for attempt in $(seq 1 100); do
        screen_text=$(kitty @ --to "unix:$socket" get-text --match all --extent screen 2>/dev/null || true)
        if [[ $screen_text == *'1.25×'* ]]; then
            break
        fi
        sleep 0.01
    done
    if [[ $screen_text != *'1.25×'* ]]; then
        echo "visual regression: $mode stale-atlas fixture did not navigate" >&2
        return 1
    fi
    if [[ $screen_text == *'loading'* ]]; then
        echo "visual regression: $mode entered preview with a stale atlas" >&2
        return 1
    fi

    local frame frame_number screenshot
    for frame_number in $(seq 0 5); do
        frame=$(printf '%02d' "$frame_number")
        screenshot="$artifact_dir/stale-$mode-transition-$frame.png"
        XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)} \
            WAYLAND_DISPLAY="$wayland_display" grim -o "$output_name" "$screenshot"
        if [[ $(classify_frame "$screenshot") != magenta ]]; then
            echo "visual regression: $mode exposed stale atlas pixels: $screenshot" >&2
            return 1
        fi
    done

    magick "$current" "$artifact_dir/stale-$mode-transition-00.png" \
        "$artifact_dir/stale-$mode-transition-05.png" -thumbnail 512x320 \
        -gravity center -extent 512x320 +append "$artifact_dir/stale-$mode-montage.png"
    kitty @ --to "unix:$socket" send-key --match all q >/dev/null 2>&1 || true
    sleep 0.2
    kitty @ --to "unix:$socket" close-window --match all >/dev/null 2>&1 || true
    sleep 0.2
    echo "visual regression: $mode stale atlas passed"
}

run_quality_case() {
    local socket="$temporary_dir/quality.sock"
    local log="$artifact_dir/quality-kitty.log"
    local tmux_server="termway-visual-$PPID-quality"
    kitty_sockets+=("$socket")
    tmux_servers+=("$tmux_server")
    WAYLAND_DISPLAY="$wayland_display" kitty --detach --start-as=fullscreen \
        --detached-log="$log" --listen-on="unix:$socket" \
        -o allow_remote_control=yes --class termway-vtest --title termway-visual-quality \
        tmux -L "$tmux_server" -f /dev/null new-session \
        "tmux set-option -g allow-passthrough all; exec '$binary' quality-fixture --tmux-bandwidth-mbps 40"
    wait_for_kitty "$socket"

    local attempt screen_text=""
    for attempt in $(seq 1 200); do
        screen_text=$(kitty @ --to "unix:$socket" get-text --match all --extent screen 2>/dev/null || true)
        if [[ $screen_text == *'1.00×'* ]]; then
            break
        fi
        sleep 0.01
    done
    if [[ $screen_text != *'1.00×'* ]]; then
        echo "visual regression: quality fixture did not initialize" >&2
        return 1
    fi

    # The fixture is intentionally hard to compress. A viewport key must still update terminal
    # chrome between 4 KiB APC chunks instead of waiting behind the complete atlas payload.
    kitty @ --to "unix:$socket" send-key --match all plus
    for attempt in $(seq 1 100); do
        screen_text=$(kitty @ --to "unix:$socket" get-text --match all --extent screen 2>/dev/null || true)
        if [[ $screen_text == *'1.25×'* ]]; then
            break
        fi
        sleep 0.01
    done
    if [[ $screen_text != *'1.25×'* ]]; then
        echo "visual regression: viewport control was blocked behind the atlas upload" >&2
        return 1
    fi
    kitty @ --to "unix:$socket" send-key --match all minus
    sleep 3

    local step
    for step in $(seq 1 7); do
        kitty @ --to "unix:$socket" send-key --match all plus
    done
    sleep 2

    kitty @ --to "unix:$socket" send-key --match all 0
    screen_text=""
    for attempt in $(seq 1 100); do
        screen_text=$(kitty @ --to "unix:$socket" get-text --match all --extent screen 2>/dev/null || true)
        if [[ $screen_text == *'1.00×'* && $screen_text == *'1080p loading'* ]]; then
            break
        fi
        sleep 0.01
    done
    if [[ $screen_text != *'1.00×'* || $screen_text != *'1080p loading'* ]]; then
        echo "visual regression: quality fixture did not reach the 1x atlas preview" >&2
        return 1
    fi

    local preview="$artifact_dir/quality-preview.png"
    local final="$artifact_dir/quality-final.png"
    XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)} \
        WAYLAND_DISPLAY="$wayland_display" grim -o "$output_name" "$preview"
    sleep 2
    XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)} \
        WAYLAND_DISPLAY="$wayland_display" grim -o "$output_name" "$final"
    screen_text=$(kitty @ --to "unix:$socket" get-text --match all --extent screen)
    if [[ $screen_text != *'1.00×'* || $screen_text != *'1080p Fast'* ]]; then
        echo "visual regression: quality fixture did not settle at the full-resolution atlas" >&2
        return 1
    fi

    magick "$preview" -crop '100%x85%+0+0' +repage "$artifact_dir/quality-preview-image.png"
    magick "$final" -crop '100%x85%+0+0' +repage "$artifact_dir/quality-final-image.png"
    local contrast
    contrast=$(magick "$artifact_dir/quality-preview-image.png" -format '%[fx:standard_deviation]' info:)
    if ! awk -v contrast="$contrast" 'BEGIN { exit !(contrast > 0.15) }'; then
        echo "visual regression: quality fixture image is missing or lacks test detail" >&2
        return 1
    fi
    if ! cmp -s "$artifact_dir/quality-preview-image.png" "$artifact_dir/quality-final-image.png"; then
        echo "visual regression: the delayed refine degraded the 1x atlas" >&2
        return 1
    fi
    magick "$preview" "$final" -thumbnail 512x320 -gravity center -extent 512x320 \
        +append "$artifact_dir/quality-montage.png"
    kitty @ --to "unix:$socket" send-key --match all q >/dev/null 2>&1 || true
    sleep 0.2
    echo "visual regression: tmux quality monotonicity passed"
}

run_case direct
run_case tmux
run_stale_atlas_case direct
run_stale_atlas_case tmux
run_quality_case
echo "visual regression: artifacts saved in $artifact_dir"
