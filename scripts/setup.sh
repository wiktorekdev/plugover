#!/bin/bash
# Sets everything up in one go: the relay on another machine over ssh (asks for its sudo password
# once), then the host daemon and the punktfunk plugin on this one.
#   scripts/setup.sh <relay-ssh-host> <lan-cidr>      e.g. simrig 192.168.1.0/24
set -euo pipefail
relay=${1:?usage: scripts/setup.sh <relay-ssh-host> <lan-cidr>}
lan=${2:?usage: scripts/setup.sh <relay-ssh-host> <lan-cidr>}
cd "$(dirname "$0")/.."
ls dist/plugover-*-linux-x86_64.tar.gz >/dev/null 2>&1 || scripts/release.sh
bundle=$(ls -t dist/plugover-*-linux-x86_64.tar.gz | head -1)
name=$(basename "$bundle" .tar.gz)

echo "== Relay on $relay"
scp -q "$bundle" "$relay:"
ssh -t "$relay" "tar xzf $name.tar.gz && old=\$HOME/plugover-test/relay.toml && sudo ./$name/install.sh relay --lan $lan \$([ -f \"\$old\" ] && echo \"\$old\")"

echo "== This computer"
tar -C dist -xzf "$bundle"
"dist/$name/install.sh" host
echo "Done. Open the punktfunk console, go to \"USB devices\" and enter the code printed above."
