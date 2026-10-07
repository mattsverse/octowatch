#!/usr/bin/env bash
# Isolated X11 lifecycle regression: requires Xvfb, Openbox, xdotool, wmctrl,
# xprop, xwininfo, dbus-run-session and a built Linux binary.
set -euo pipefail
binary=$(realpath "${1:?usage: $0 /path/to/octowatcher}")
if [[ ${OCTOWATCHER_REOPEN_TEST_SESSION:-} != 1 ]]; then
    exec dbus-run-session -- env OCTOWATCHER_REOPEN_TEST_SESSION=1 "$0" "$binary"
fi
workspace=$(mktemp -d)
app_pid= wm_pid= xvfb_pid=
cleanup() {
    for pid in "$app_pid" "$wm_pid" "$xvfb_pid"; do
        [[ -z $pid ]] || kill "$pid" 2>/dev/null || true
    done
    rm -rf "$workspace"
}
trap cleanup EXIT
export HOME="$workspace/home" XDG_CONFIG_HOME="$workspace/config" XDG_RUNTIME_DIR="$workspace/runtime"
unset WAYLAND_DISPLAY GH_TOKEN GITHUB_TOKEN GH_ENTERPRISE_TOKEN GITHUB_ENTERPRISE_TOKEN
export GH_CONFIG_DIR="$workspace/gh-config"
mkdir -m 700 "$HOME" "$XDG_CONFIG_HOME" "$XDG_RUNTIME_DIR"
mkdir -p "$XDG_CONFIG_HOME/octowatcher" "$workspace/empty" "$workspace/bin"
# No repository discovery, notifications or GitHub authentication in this test.
printf '{"roots":["%s"],"request_auth":true}\n' "$workspace/empty" > "$XDG_CONFIG_HOME/octowatcher/state.json"
printf '#!/bin/sh\nexit 1\n' > "$workspace/bin/gh"
chmod +x "$workspace/bin/gh"
export PATH="$workspace/bin:$PATH"
Xvfb -displayfd 3 -screen 0 1024x768x24 3>"$workspace/display" >"$workspace/xvfb.log" 2>&1 &
xvfb_pid=$!
await() {
    local attempt
    for attempt in {1..100}; do
        if "$@"; then return 0; fi
        sleep 0.05
    done
    echo "timed out: $*" >&2
    cat "$workspace/app.log" 2>/dev/null || true
    return 1
}
await test -s "$workspace/display"
export DISPLAY=":$(cat "$workspace/display")"
openbox >"$workspace/wm.log" 2>&1 &
wm_pid=$!
wm_ready() { xprop -root _NET_SUPPORTING_WM_CHECK 2>/dev/null | grep -q 'window id'; }
await wm_ready
"$binary" --background >"$workspace/app.log" 2>&1 &
app_pid=$!
window_for_app() { xdotool search --onlyvisible --pid "$app_pid" 2>/dev/null | head -n 1; }
window_exists() { [[ -n $(window_for_app) ]]; }
await test -s "$HOME/.octowatcher-instance/instance.lock"
# Allow the cold launch to finish initialization on the virtual display.
sleep 1
kill -0 "$app_pid"
[[ -z $(window_for_app) ]]
"$binary" --background
[[ -z $(window_for_app) ]]
"$binary"
await window_exists
old=$(window_for_app)
# The actual WM Close action invokes our minimize-on-close handler.
wmctrl -ic "$(printf '0x%x' "$old")"
is_minimized() { xprop -id "$old" WM_STATE 2>/dev/null | grep -q Iconic; }
await is_minimized
kill -0 "$app_pid"
"$binary" --background
is_minimized
# A repeated ordinary launch must return and reveal one window of the same PID.
"$binary"
restored() {
    local current
    current=$(window_for_app)
    [[ -n $current ]] &&
        xwininfo -id "$current" 2>/dev/null | grep -q 'Map State: IsViewable'
}
await restored
kill -0 "$app_pid"
[[ $(xdotool search --onlyvisible --pid "$app_pid" | wc -l) -eq 1 ]]
# Test the native minimize button path too, without invoking Close.
old=$(window_for_app)
xdotool windowminimize "$old"
await is_minimized
"$binary"
await restored
kill -0 "$app_pid"
[[ $(xdotool search --onlyvisible --pid "$app_pid" | wc -l) -eq 1 ]]
echo 'PASS: quiet cold launch, Close and native minimize reopen in the same process'
