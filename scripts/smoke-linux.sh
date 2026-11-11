#!/usr/bin/env bash
# Run inside an X11 session, e.g. xvfb-run -a bash scripts/smoke-linux.sh BINARY.
set -euo pipefail
binary="${1:-target/release/pc_activity_scanner}"
config_dir="$(mktemp -d)"
app_pid=""
cleanup() {
    if [[ -n "$app_pid" ]]; then
        kill "$app_pid" 2>/dev/null || true
        wait "$app_pid" 2>/dev/null || true
    fi
    rm -rf "$config_dir"
}
trap cleanup EXIT
XDG_CONFIG_HOME="$config_dir/config" XDG_DATA_HOME="$config_dir/data" "$binary" >"$config_dir/app.log" 2>&1 &
app_pid=$!
window=""
for _ in {1..50}; do
    if ! kill -0 "$app_pid" 2>/dev/null; then
        cat "$config_dir/app.log"
        echo 'Application exited before its window appeared.' >&2
        exit 1
    fi
    window="$(xdotool search --onlyvisible --pid "$app_pid" 2>/dev/null | head -n 1 || true)"
    [[ -n "$window" ]] && break
    sleep 0.2
done
if [[ -z "$window" ]]; then
    cat "$config_dir/app.log"
    echo 'Application window did not appear within 10 seconds.' >&2
    exit 1
fi
xdotool windowsize "$window" 960 600
sleep 3
kill -0 "$app_pid"
if [[ -n "${SCANNER_SCREENSHOT:-}" ]]; then
    import -window "$window" "$SCANNER_SCREENSHOT"
fi
printf 'Linux window startup and resize passed (PID %s).\n' "$app_pid"
