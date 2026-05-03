#!/usr/bin/env bash
# Build the wiredesk-term release binary. No bundling needed — it's a
# pure CLI you run inside Ghostty / iTerm / Terminal.app.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

BIN="target/release/wiredesk-term"

echo "==> Building wiredesk-term…"
cargo build --release -p wiredesk-term

if [[ ! -f "$BIN" ]]; then
    echo "ERROR: $BIN not found after cargo build" >&2
    exit 1
fi

echo
echo "==> Done. Binary: ${REPO_ROOT}/${BIN}"
echo "   Run directly: ./${BIN}"
echo "   With alias (from ~/.zshrc): wd"
echo "   Mind the mutual exclusion: quit WireDesk.app first."
