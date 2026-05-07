#!/usr/bin/env bash
# All-in-one: kill any running WireDesk on this Mac, rebuild the .app,
# launch it from the terminal with RUST_LOG=debug and a tee'd log to
# /tmp/wiredesk-mac.log. Run from anywhere — script changes cwd to the
# repo root.
#
# Usage:
#   ./scripts/run-mac.sh                   # default: RUST_LOG=info
#   RUST_LOG=debug ./scripts/run-mac.sh    # override logging level
#   ./scripts/run-mac.sh --no-build        # skip rebuild, just kill+run

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

LOG_FILE="${LOG_FILE:-/tmp/wiredesk-mac.log}"
RUST_LOG="${RUST_LOG:-info,btleplug=info}"
SKIP_BUILD=0

for arg in "$@"; do
    case "$arg" in
        --no-build) SKIP_BUILD=1 ;;
        *) echo "Unknown arg: $arg" >&2; exit 1 ;;
    esac
done

echo "==> Killing any running WireDesk processes…"
# Kill both the .app GUI binary and the standalone terminal binary.
# `pkill -f` matches the full command line so it catches both the
# direct-run case and the .app-launched-via-LaunchServices case.
# `|| true` keeps set -e happy when nothing matches.
pkill -f "WireDesk.app/Contents/MacOS/wiredesk-client" 2>/dev/null || true
pkill -x "wiredesk-client"                              2>/dev/null || true
pkill -x "wiredesk-term"                                2>/dev/null || true

# Brief settle so the OS releases the BT-adapter / IPC socket / Accessibility
# handle the previous instance held. 200ms is plenty in practice.
sleep 0.3

if [[ "$SKIP_BUILD" -eq 0 ]]; then
    echo "==> Rebuilding WireDesk.app…"
    ./scripts/build-mac-app.sh
fi

BIN="$REPO_ROOT/target/release/WireDesk.app/Contents/MacOS/wiredesk-client"
if [[ ! -x "$BIN" ]]; then
    echo "ERROR: $BIN not found or not executable. Run without --no-build first." >&2
    exit 1
fi

echo "==> Launching with RUST_LOG=$RUST_LOG"
echo "    Logs tee'd to $LOG_FILE"
echo "    Ctrl+C here to stop."
echo

exec env RUST_LOG="$RUST_LOG" "$BIN" 2>&1 | tee "$LOG_FILE"
