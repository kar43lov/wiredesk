#!/usr/bin/env bash
# Assemble target/release/WireDesk.app from the wiredesk-client release binary
# plus Info.plist and a generated AppIcon.icns. Idempotent: re-running
# overwrites the bundle in place.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

APP_NAME="WireDesk"
APP_BUNDLE="target/release/${APP_NAME}.app"
BIN_NAME="wiredesk-client"
PLIST_SRC="apps/wiredesk-client/Info.plist"
ICON_SRC="assets/icon-source.png"

echo "==> Building release binary…"
cargo build --release -p wiredesk-client

if [[ ! -f "target/release/${BIN_NAME}" ]]; then
    echo "ERROR: target/release/${BIN_NAME} not found after cargo build" >&2
    exit 1
fi
if [[ ! -f "$PLIST_SRC" ]]; then
    echo "ERROR: ${PLIST_SRC} not found" >&2
    exit 1
fi

echo "==> Assembling .app bundle at ${APP_BUNDLE}…"
rm -rf "$APP_BUNDLE"
mkdir -p "${APP_BUNDLE}/Contents/MacOS" "${APP_BUNDLE}/Contents/Resources"

cp "target/release/${BIN_NAME}" "${APP_BUNDLE}/Contents/MacOS/${BIN_NAME}"
cp "$PLIST_SRC" "${APP_BUNDLE}/Contents/Info.plist"

if [[ -f "$ICON_SRC" ]]; then
    echo "==> Generating AppIcon.icns from ${ICON_SRC}…"
    ICONSET_DIR="$(mktemp -d)/AppIcon.iconset"
    mkdir -p "$ICONSET_DIR"

    # iconutil expects these specific sizes for a complete .icns set.
    declare -a SIZES=(16 32 64 128 256 512 1024)
    for sz in "${SIZES[@]}"; do
        sips -z "$sz" "$sz" "$ICON_SRC" --out "${ICONSET_DIR}/icon_${sz}x${sz}.png" >/dev/null
    done
    # @2x retina variants — same pixel data, different filename.
    cp "${ICONSET_DIR}/icon_32x32.png"     "${ICONSET_DIR}/icon_16x16@2x.png"
    cp "${ICONSET_DIR}/icon_64x64.png"     "${ICONSET_DIR}/icon_32x32@2x.png"
    cp "${ICONSET_DIR}/icon_256x256.png"   "${ICONSET_DIR}/icon_128x128@2x.png"
    cp "${ICONSET_DIR}/icon_512x512.png"   "${ICONSET_DIR}/icon_256x256@2x.png"
    cp "${ICONSET_DIR}/icon_1024x1024.png" "${ICONSET_DIR}/icon_512x512@2x.png"
    # iconutil rejects unknown filenames, so drop the bare 64×64 / 1024×1024.
    rm "${ICONSET_DIR}/icon_64x64.png" "${ICONSET_DIR}/icon_1024x1024.png"

    iconutil --convert icns "$ICONSET_DIR" --output "${APP_BUNDLE}/Contents/Resources/AppIcon.icns"
    rm -rf "$ICONSET_DIR"
else
    echo "WARN: ${ICON_SRC} not found — bundle will use default icon."
fi

# Ensure binary is executable (cargo already does this, but be defensive).
chmod +x "${APP_BUNDLE}/Contents/MacOS/${BIN_NAME}"

# Codesign the bundle. Two reasons: Dock / Launch Services treat each signed
# rebuild as a distinct bundle (otherwise a stale icon cache leaves the Dock
# with the generic executable icon mid-run), and macOS keys the Accessibility
# grant to the code signature.
#
# With the default ad-hoc signature ("-") every rebuild gets a fresh cdhash,
# so the grant is lost and the app lands on the permission screen again. A
# self-signed "WireDesk Dev" certificate gives a designated requirement that
# is stable across builds (identifier + certificate), and the grant survives.
# Create it once with scripts/make-dev-signing-cert.sh; it is picked up here
# automatically, or point WIREDESK_SIGN_IDENTITY at any other identity.
SIGN_IDENTITY="${WIREDESK_SIGN_IDENTITY:-}"
if [[ -z "$SIGN_IDENTITY" ]] \
   && security find-identity -v -p codesigning 2>/dev/null | grep -q '"WireDesk Dev"'; then
    SIGN_IDENTITY="WireDesk Dev"
fi
if [[ -n "$SIGN_IDENTITY" ]]; then
    echo "==> Codesigning bundle with identity '${SIGN_IDENTITY}'…"
    codesign --force --deep --sign "$SIGN_IDENTITY" "${APP_BUNDLE}"
else
    echo "==> Ad-hoc codesigning bundle (Accessibility grant will not survive this rebuild;"
    echo "    run scripts/make-dev-signing-cert.sh once to keep it)…"
    codesign --force --deep --sign - "${APP_BUNDLE}" 2>&1 | grep -v "replacing existing signature" || true
fi

# Refresh Launch Services registration so Dock + Finder pick up the new
# bundle metadata immediately rather than next mds reindex.
LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
if [[ -x "$LSREGISTER" ]]; then
    "$LSREGISTER" -f "${APP_BUNDLE}" 2>/dev/null || true
fi

echo
echo "==> Done. Bundle: ${REPO_ROOT}/${APP_BUNDLE}"
echo "   Open with: open '${APP_BUNDLE}'"
echo "   First launch: Gatekeeper will block — right-click → Open, then confirm."
echo "   If Dock icon stays generic: killall Dock"
