#!/bin/bash
# Builds everything:
#   dist/plugover-<version>-linux-x86_64.tar.gz   relay + Linux host (install.sh)
#   dist/windows/                                 Windows host, served on the LAN for install.ps1
# PLUGOVER_BASE is the URL the Windows files are downloaded from (default: the latest GitHub release).
set -euo pipefail
cd "$(dirname "$0")/.."
base=${PLUGOVER_BASE:-https://github.com/wiktorekdev/plugover/releases/latest/download}
version=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
plugin_version=$(grep -m1 '"version"' plugin/package.json | cut -d'"' -f4)
usbip_version=0.9.8.0
usbip_sha=81f426741f7ee2ed991febe24a22daca8400b6ae2f171054e3fb404897e15d39
usbip_file=USBip-$usbip_version-x64.exe

# Build paths would otherwise end up in panic messages.
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$HOME=~"
cargo build --release --target x86_64-unknown-linux-musl
cargo zigbuild --release --target x86_64-pc-windows-gnu
(cd plugin && bun install --frozen-lockfile && bun run build && rm -f ./*.tgz && bun pm pack)
plugin=plugin/punktfunk-plugin-plugover-$plugin_version.tgz

out=dist/plugover-$version-linux-x86_64
rm -rf "$out" && mkdir -p "$out"
cp target/x86_64-unknown-linux-musl/release/plugover "$plugin" packaging/install.sh packaging/plugover-* "$out/"
tar -C dist -czf "$out.tar.gz" "$(basename "$out")"

mkdir -p dist/cache
[ -f "dist/cache/$usbip_file" ] ||
    curl -fL -o "dist/cache/$usbip_file" "https://github.com/vadimgrn/usbip-win2/releases/download/v.$usbip_version/$usbip_file"
echo "$usbip_sha  dist/cache/$usbip_file" | sha256sum -c --quiet
win=dist/windows
rm -rf "$win" && mkdir -p "$win"
cp target/x86_64-pc-windows-gnu/release/plugover.exe "$plugin" "dist/cache/$usbip_file" "$win/"
sha() { sha256sum "$1" | cut -d' ' -f1; }
sed -e "s#__BASE__#$base#g" \
    -e "s#__PLUGIN_VERSION__#$plugin_version#g" \
    -e "s#__USBIP_VERSION__#$usbip_version#g" \
    -e "s#__SHA_EXE__#$(sha "$win/plugover.exe")#" \
    -e "s#__SHA_PLUGIN__#$(sha "$win/$(basename "$plugin")")#" \
    -e "s#__SHA_USBIP__#$usbip_sha#" \
    packaging/install.ps1 > "$win/install.ps1"
! grep -n '__[A-Z_]*__' "$win/install.ps1"
echo "$out.tar.gz"
echo "$win/ (Windows: irm $base/install.ps1 | iex)"
