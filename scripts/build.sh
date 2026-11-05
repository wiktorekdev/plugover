#!/bin/bash
# Builds the release files into dist/:
#   plugover-<version>-linux-{x86_64,aarch64,armv7}   Linux binaries; `plugover setup` installs the relay or host
#   plugover-<version>-windows-x86_64.exe    the same for Windows (double-click runs setup)
#   installer.sh, installer.ps1              download the binary of this release and run setup
#   plugover_<version>_{amd64,arm64,armhf}.deb   Debian packages
#   winget/                                  manifests for a microsoft/winget-pkgs pull request
#   SHA256SUMS, SHA256SUMS.sig                checksums, signed with the release key
# The console plugin is built first and goes inside every binary.
set -euo pipefail
cd "$(dirname "$0")/.."
version=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
plugin_version=$(grep -m1 '"version"' plugin/package.json | cut -d'"' -f4)
[ "$version" = "$plugin_version" ] || { echo "Cargo.toml is $version but plugin/package.json is $plugin_version"; exit 1; }

(cd plugin && bun install --frozen-lockfile && bun run build && rm -f ./*.tgz && bun pm pack)
export PLUGOVER_PLUGIN=$PWD/plugin/punktfunk-plugin-plugover-$version.tgz
# Build paths would otherwise end up in panic messages.
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$HOME=~"
# Every target is a static musl build except Windows. ARM needs a writable zig lib dir because
# cargo-zigbuild patches a header in it.
zig_lib=$(zig env | sed -nE 's/.*lib_dir"? *[:=] *"([^"]*)".*/\1/p')
if [ ! -w "$zig_lib" ]; then
    export ZIG_LIB_DIR=${ZIG_LIB_DIR:-/var/tmp/plugover-zig-lib}
    [ -d "$ZIG_LIB_DIR" ] || cp -r "$zig_lib" "$ZIG_LIB_DIR"
fi
declare -A linux=([x86_64]=x86_64-unknown-linux-musl [aarch64]=aarch64-unknown-linux-musl [armv7]=armv7-unknown-linux-musleabihf)
declare -A debian=([x86_64]=amd64 [aarch64]=arm64 [armv7]=armhf)
for arch in "${!linux[@]}"; do cargo zigbuild --release --target "${linux[$arch]}"; done
cargo zigbuild --release --target x86_64-pc-windows-gnu

rm -rf dist && mkdir -p dist/winget
for arch in "${!linux[@]}"; do cp "target/${linux[$arch]}/release/plugover" "dist/plugover-$version-linux-$arch"; done
windows=plugover-$version-windows-x86_64.exe
cp target/x86_64-pc-windows-gnu/release/plugover.exe "dist/$windows"

sha() { sha256sum "$1" | cut -d' ' -f1; }
win_sha=$(sha "dist/$windows")
fill() {
    sed -e "s#__VERSION__#$version#g" -e "s#__SHA_WINDOWS__#$win_sha#" -e "s#__SHA_WINDOWS_UPPER__#${win_sha^^}#" \
        -e "s#__SHA_X86_64__#$(sha "dist/plugover-$version-linux-x86_64")#" \
        -e "s#__SHA_AARCH64__#$(sha "dist/plugover-$version-linux-aarch64")#" \
        -e "s#__SHA_ARMV7__#$(sha "dist/plugover-$version-linux-armv7")#" \
        -e "s#__ARCH__#${deb_arch:-}#" "$1" >"$2"
    if grep -n '__[A-Z_0-9]*__' "$2"; then exit 1; fi
}
fill packaging/installer.sh dist/installer.sh
fill packaging/installer.ps1 dist/installer.ps1

# Packages. Nothing here publishes them; the winget manifests go to winget-pkgs by hand.
for f in packaging/winget/*.yaml; do fill "$f" "dist/winget/$(basename "$f")"; done
for arch in "${!linux[@]}"; do
    root=target/deb/${debian[$arch]}
    rm -rf "$root" && mkdir -p "$root/DEBIAN" "$root/usr/bin" "$root/usr/share/doc/plugover"
    deb_arch=${debian[$arch]} fill packaging/deb/control "$root/DEBIAN/control"
    install -m755 packaging/deb/postinst packaging/deb/prerm "$root/DEBIAN/"
    install -m755 "dist/plugover-$version-linux-$arch" "$root/usr/bin/plugover"
    install -m644 LICENSE "$root/usr/share/doc/plugover/copyright"
    dpkg-deb --root-owner-group -Zxz --build "$root" "dist/plugover_${version}_${debian[$arch]}.deb" >/dev/null
done

(cd dist && sha256sum -- plugover* installer.* >SHA256SUMS)
# `plugover update` installs only releases signed with this key (its public half is
# packaging/release-key.pem, and in src/setup/mod.rs). Keep a backup: without it, no update can be
# signed and every install would have to be updated by hand.
key=${PLUGOVER_SIGNING_KEY:-$HOME/.config/plugover-release/key.pem}
if [ -r "$key" ]; then
    openssl pkeyutl -sign -inkey "$key" -rawin -in dist/SHA256SUMS -out dist/SHA256SUMS.sig
    openssl pkeyutl -verify -pubin -inkey packaging/release-key.pem -rawin -in dist/SHA256SUMS -sigfile dist/SHA256SUMS.sig >/dev/null
else
    echo "warning: no signing key at $key, so SHA256SUMS is unsigned and plugover update will refuse this release" >&2
fi
find dist -type f | sort
