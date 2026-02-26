#!/bin/bash
# scripts/threshold-wrapper.sh — restart loop with build support
#
# This wrapper runs the Threshold daemon in a loop, handling:
# - Stop sentinel: exits cleanly when `threshold daemon stop` is used
# - Restart pending: optionally rebuilds before restart
# - Crash restart: waits 5s before restarting on unexpected exits
# - Supervised marker: writes PID/timestamp for CLI detection
#
# Used by launchd (via plist) or run directly for supervised mode.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DATA_DIR="${THRESHOLD_DATA_DIR:-$HOME/.threshold}"
STATE_DIR="$DATA_DIR/state"

mkdir -p "$STATE_DIR"

# Write supervised marker with wrapper PID and start time so CLI can verify liveness
echo "{\"wrapper_pid\": $$, \"started_at\": \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\"}" > "$STATE_DIR/supervised"

cleanup() {
    rm -f "$STATE_DIR/supervised"
}
trap cleanup EXIT

BINARY="$REPO_ROOT/target/debug/threshold"

# Initial build if binary doesn't exist (first boot, after cargo clean, etc.)
if [ ! -f "$BINARY" ]; then
    echo "[wrapper] Binary not found at $BINARY. Building from source..."
    (cd "$REPO_ROOT" && cargo build -p threshold) || {
        echo "[wrapper] Initial build failed. Cannot start daemon. Exiting."
        exit 1
    }
fi

while true; do
    # Check for stop sentinel — exit loop instead of restarting
    if [ -f "$STATE_DIR/stop-sentinel" ]; then
        rm -f "$STATE_DIR/stop-sentinel"
        echo "[wrapper] Stop sentinel found. Exiting."
        break
    fi

    # Check if a restart was requested via CLI
    # The CLI builds BEFORE sending SIGTERM, so skip_build is normally true.
    # The wrapper retains build capability as a fallback for manual restarts
    # (e.g., someone kills the daemon directly, or the daemon crashes).
    if [ -f "$STATE_DIR/restart-pending.json" ]; then
        SKIP_BUILD=$(python3 -c "
import json, sys
try:
    d = json.load(open('$STATE_DIR/restart-pending.json'))
    print(str(d.get('skip_build', False)).lower())
except: print('false')
" 2>/dev/null || echo "false")
        rm -f "$STATE_DIR/restart-pending.json"

        if [ "$SKIP_BUILD" != "true" ]; then
            echo "[wrapper] Building from source..."
            (cd "$REPO_ROOT" && cargo build -p threshold) || {
                echo "[wrapper] Build failed. Starting with existing binary."
            }
        else
            echo "[wrapper] Build already completed by CLI. Skipping rebuild."
        fi
    fi

    echo "[wrapper] Starting daemon..."
    EXIT_CODE=0
    "$BINARY" daemon start || EXIT_CODE=$?

    # Check for stop sentinel again (may have been written during shutdown)
    if [ -f "$STATE_DIR/stop-sentinel" ]; then
        rm -f "$STATE_DIR/stop-sentinel"
        echo "[wrapper] Stop sentinel found after exit. Exiting."
        break
    fi

    if [ $EXIT_CODE -ne 0 ] && [ ! -f "$STATE_DIR/restart-pending.json" ]; then
        echo "[wrapper] Daemon exited with code $EXIT_CODE. Waiting 5s before restart..."
        sleep 5
    fi
done

# Exit 0 = intentional stop (launchd won't restart).
# Exit 1 = unexpected failure (launchd will restart via KeepAlive/SuccessfulExit=false).
echo "[wrapper] Wrapper exiting."
exit 0
