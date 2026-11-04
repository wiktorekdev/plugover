#!/bin/sh
# Plugover for Linux:
#   curl -fsSL https://github.com/wiktorekdev/plugover/releases/latest/download/installer.sh | sh
# Downloads the plugover binary of this release, checks it, and runs `plugover setup`, which asks
# whether this computer is the relay or the host. Options go to setup:
#   ... | sh -s -- --relay --lan 192.168.1.0/24 --yes
set -eu

version=__VERSION__
[ "$(uname -s)" = Linux ] || { echo "This installer is for Linux; on Windows use installer.ps1." >&2; exit 1; }
case $(uname -m) in
    x86_64 | amd64) arch=x86_64 sha=__SHA_X86_64__ ;;
    aarch64 | arm64) arch=aarch64 sha=__SHA_AARCH64__ ;;
    armv7* | armv8l) arch=armv7 sha=__SHA_ARMV7__ ;;
    *) echo "Plugover has no build for $(uname -m) yet (there are x86_64, aarch64 and armv7)." >&2; exit 1 ;;
esac
file=plugover-$version-linux-$arch
url=https://github.com/wiktorekdev/plugover/releases/download/v$version/$file

dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
echo "Downloading Plugover $version..."
if command -v curl >/dev/null 2>&1; then
    curl -fsSL -o "$dir/plugover" "$url"
else
    wget -qO "$dir/plugover" "$url"
fi
echo "$sha  $dir/plugover" | sha256sum -c --quiet || { echo "The download does not match its checksum; nothing was installed." >&2; exit 1; }
chmod +x "$dir/plugover"

# Under `curl | sh` stdin is this script; setup reads its answers from the terminal.
if [ -t 1 ] && (: </dev/tty) 2>/dev/null; then
    "$dir/plugover" setup "$@" </dev/tty
else
    "$dir/plugover" setup "$@"
fi
