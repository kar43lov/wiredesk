#!/usr/bin/env bash
# Build both Mac client (.app bundle) and wiredesk-term CLI in one shot.
# Just chains the two single-target scripts so they stay independent.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

bash "$REPO_ROOT/scripts/build-mac-app.sh"
echo
bash "$REPO_ROOT/scripts/build-mac-term.sh"
echo
echo "==> Both targets built."
