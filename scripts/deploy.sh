#!/bin/bash
# Installs a local build in one go: the relay on another machine over ssh (asks for its sudo
# password once), then the host daemon and the punktfunk plugin on this one.
#   scripts/deploy.sh <relay-ssh-host> <lan-cidr>      e.g. simrig 192.168.1.0/24
set -euo pipefail
relay=${1:?usage: scripts/deploy.sh <relay-ssh-host> <lan-cidr>}
lan=${2:?usage: scripts/deploy.sh <relay-ssh-host> <lan-cidr>}
cd "$(dirname "$0")/.."
scripts/build.sh
arch() { case $1 in aarch64 | arm64) echo aarch64 ;; armv7* | armv8l) echo armv7 ;; *) echo x86_64 ;; esac; }
bin=$(find dist -name "plugover-*-linux-$(arch "$(uname -m)")" | head -1)
relay_bin=$(find dist -name "plugover-*-linux-$(arch "$(ssh "$relay" uname -m)")" | head -1)

echo "== Relay on $relay"
scp -q "$relay_bin" "$relay:plugover-setup"
ssh -t "$relay" "./plugover-setup setup --relay --lan $lan --yes; rm -f plugover-setup"

echo "== This computer"
"$bin" setup --host --yes
