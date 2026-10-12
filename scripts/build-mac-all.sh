#!/usr/bin/env bash
# Build both Mac client (.app bundle) and wiredesk-term CLI in one shot.
# Just chains the two single-target scripts so they stay independent.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

bash "$REPO_ROOT/scripts/build-mac-app.sh"
echo
bash "$REPO_ROOT/scripts/build-mac-term.sh"
cargo build --manifest-path "$REPO_ROOT/Cargo.toml" --release -p wiredesk-browser-cli
echo
echo "==> Both targets built."
