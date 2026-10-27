#!/bin/bash
# Installs Plugover from a release bundle (or a source checkout after scripts/release.sh).
#   sudo ./install.sh relay [--lan CIDR]
#        where the devices are plugged in. --lan opens TCP 7275 and mDNS for that network in an
#        nftables "inet filter input" chain, now and in /etc/nftables.conf.
#   ./install.sh host                          on the punktfunk host, as your user
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
bin=$here/plugover
[ -x "$bin" ] || bin=$here/../../target/x86_64-unknown-linux-musl/release/plugover
plugin=$(ls "$here"/punktfunk-plugin-plugover-*.tgz "$here"/../../plugin/punktfunk-plugin-plugover-*.tgz 2>/dev/null | head -1 || true)

open_firewall() {
    local lan=$1 conf=/etc/nftables.conf
    if ! nft list chain inet filter input >/dev/null 2>&1; then
        echo "No nftables 'inet filter input' chain here; open TCP 7275 and UDP 5353 for $lan yourself."
        return
    fi
    for rule in "tcp dport 7275" "udp dport 5353"; do
        nft list chain inet filter input | grep -q "$rule ip saddr $lan accept comment \"plugover\"" ||
            nft insert rule inet filter input $rule ip saddr "$lan" accept comment '"plugover"'
    done
    if [ -f "$conf" ] && ! grep -q 'comment "plugover"' "$conf"; then
        cp "$conf" "$conf.plugover-backup"
        # Right after the first input hook, so nothing earlier in the chain can drop it.
        awk -v lan="$lan" '{ print } !done && /hook input/ {
            print "    tcp dport 7275 ip saddr " lan " accept comment \"plugover\""
            print "    udp dport 5353 ip saddr " lan " accept comment \"plugover\""
            done = 1 }' "$conf.plugover-backup" > "$conf"
        if nft -c -f "$conf"; then
            echo "Opened TCP 7275 and mDNS for $lan (also saved in $conf; backup in $conf.plugover-backup)."
        else
            cp "$conf.plugover-backup" "$conf"
            echo "Could not add the rules to $conf, left it unchanged; they are active until the next reboot."
        fi
    fi
}

case "${1:-}" in
relay)
    [ "$(id -u)" = 0 ] || { echo "run as root: sudo $0 relay"; exit 1; }
    shift
    lan=""
    if [ "${1:-}" = "--lan" ]; then lan=$2; shift 2; fi
    # A test relay started by hand would hold the port.
    pkill -x plugover 2>/dev/null && sleep 2 || true
    install -Dm755 "$bin" /usr/local/bin/plugover
    install -Dm644 "$here/plugover-relay.conf" /etc/modules-load.d/plugover-relay.conf
    install -Dm644 "$here/plugover-relay.service" /etc/systemd/system/plugover-relay.service
    modprobe usbip-host
    systemctl daemon-reload
    systemctl enable plugover-relay
    systemctl restart plugover-relay
    [ -n "$lan" ] && open_firewall "$lan"
    echo "Relay running (journalctl -u plugover-relay)."
    echo
    /usr/local/bin/plugover code
    ;;
host)
    [ "$(id -u)" != 0 ] || { echo "run as your normal user, not root"; exit 1; }
    install -Dm755 "$bin" "$HOME/.local/bin/plugover"
    install -Dm644 "$here/plugover-host.service" "$HOME/.config/systemd/user/plugover-host.service"
    systemctl --user daemon-reload
    systemctl --user enable --now plugover-host
    systemctl --user restart plugover-host
    echo "Host daemon running."
    if [ -n "$plugin" ] && command -v punktfunk-host >/dev/null; then
        # The plugin runner re-resolves packages from inside its own tree, so the tarball lives there.
        vendor=$HOME/.config/punktfunk/plugins/vendor
        install -Dm644 "$plugin" "$vendor/$(basename "$plugin")"
        punktfunk-host plugins add "$vendor/$(basename "$plugin")" --allow-public-registry
        echo "Plugin installed: open the punktfunk console and find \"USB devices\"."
    else
        echo "punktfunk-host or the plugin package was not found; skipped the console plugin."
    fi
    ;;
*)
    echo "usage: $0 relay [--lan CIDR] | host"; exit 1 ;;
esac
