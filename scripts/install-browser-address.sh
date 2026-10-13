#!/usr/bin/env bash
# One-time, explicit installation. No hosts/PF/DNS changes and no root GUI.
set -euo pipefail
if [[ "$EUID" != 0 ]]; then
    echo "Run this installer once with sudo. WireDesk itself must stay unprivileged." >&2
    exit 1
fi
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source_bin="$repo_root/target/release/wd-web-forwarder"
source_plist="$repo_root/assets/dev.kar43lov.wiredesk-web-forwarder.plist"
helper_dir="/Library/PrivilegedHelperTools"
helper="$helper_dir/dev.kar43lov.wiredesk-web-forwarder"
plist="/Library/LaunchDaemons/dev.kar43lov.wiredesk-web-forwarder.plist"
if [[ ! -f "$source_bin" || -L "$source_bin" ]]; then
    echo "Build first: cargo build --release -p wiredesk-browser-cli --bin wd-web-forwarder" >&2
    exit 1
fi
if [[ -e "$helper" || -L "$helper" || -e "$plist" || -L "$plist" ]]; then
    echo "Helper files already exist; refusing to overwrite an existing installation." >&2
    exit 1
fi
/usr/bin/plutil -lint "$source_plist"
daemon_dir="/Library/LaunchDaemons"
if [[ ! -d "$daemon_dir" || -L "$daemon_dir" || "$(/usr/bin/stat -f %u "$daemon_dir")" != 0 ]]; then
    echo "LaunchDaemons must be a root-owned real directory." >&2
    exit 1
fi
daemon_mode="$(/usr/bin/stat -f %Lp "$daemon_dir")"
if (( (8#$daemon_mode & 0022) != 0 )); then
    echo "LaunchDaemons must not be group/world writable." >&2
    exit 1
fi
if [[ -e "$helper_dir" ]]; then
    if [[ -L "$helper_dir" || "$(/usr/bin/stat -f %u "$helper_dir")" != 0 ]]; then
        echo "Helper directory must be a root-owned real directory." >&2
        exit 1
    fi
    dir_mode="$(/usr/bin/stat -f %Lp "$helper_dir")"
    if (( (8#$dir_mode & 0022) != 0 )); then
        echo "Helper directory must not be group/world writable." >&2
        exit 1
    fi
else
    /usr/bin/install -d -o root -g wheel -m 755 "$helper_dir"
fi
# Both destinations were absent. On failure remove only the two files this
# invocation created; no pre-existing configuration is overwritten.
installed_bin=false
installed_plist=false
cleanup() {
    if [[ "$?" != 0 ]]; then
        if $installed_plist; then
            /bin/launchctl bootout system "$plist" || true
            /bin/rm -f "$plist"
        fi
        if $installed_bin; then /bin/rm -f "$helper"; fi
    fi
}
trap cleanup EXIT
/usr/bin/install -o root -g wheel -m 755 "$source_bin" "$helper"
installed_bin=true
/usr/bin/install -o root -g wheel -m 644 "$source_plist" "$plist"
installed_plist=true
/bin/launchctl bootstrap system "$plist"
echo "Installed loopback HTTP socket 127.0.0.1:80; helper runs as nobody."
echo "Restart WireDesk. Browser address: http://wiredesk.localhost/"
