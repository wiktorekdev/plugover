#!/bin/bash
# One-off relay for testing on a machine without a Plugover install.
# Usage: sudo bash test-relay.sh <host-ip>
# Opens the port for that host only, runs the relay in the foreground, and on Ctrl+C
# closes the port again and hands every lent device back to its local driver.
set -euo pipefail

HOST_IP=${1:?usage: sudo bash test-relay.sh <host-ip>}
PORT=7275
DIR=$(cd "$(dirname "$0")" && pwd)
BIN=$DIR/plugover
STATE=$DIR/relay.toml
STUB=/sys/bus/usb/drivers/usbip-host
TAG=plugover-test

cleanup() {
    [ -n "${DUMPER:-}" ] && kill "$DUMPER" 2>/dev/null
    nft -a -j list ruleset | python3 -c '
import json, sys
for r in json.load(sys.stdin)["nftables"]:
    r = r.get("rule")
    if r and r.get("comment") == "'"$TAG"'":
        print(r["family"], r["table"], r["chain"], r["handle"])' |
        while read -r fam tab chain handle; do nft delete rule "$fam" "$tab" "$chain" handle "$handle"; done
    for d in "$STUB"/[0-9]*; do
        [ -e "$d" ] || continue
        b=$(basename "$d")
        echo "$b" > "$STUB/unbind"
        echo "$b" > "$STUB/rebind"
        echo "returned $b to its local driver"
    done
    echo "port $PORT closed"
}
trap cleanup EXIT

modprobe usbip-host
"$BIN" devices

# Every filter chain on the input hook must accept the traffic, so open the port in each.
nft -j list chains | python3 -c '
import json, sys
for c in json.load(sys.stdin)["nftables"]:
    c = c.get("chain")
    # The host is reached over IPv4, so ip6 chains never see this traffic.
    if c and c.get("hook") == "input" and c.get("type") == "filter" and c["family"] in ("ip", "inet"):
        print(c["family"], c["table"], c["name"])' |
    while read -r fam tab chain; do
        nft insert rule "$fam" "$tab" "$chain" ip saddr "$HOST_IP" tcp dport "$PORT" counter accept comment "\"$TAG\""
        nft insert rule "$fam" "$tab" "$chain" ip saddr "$HOST_IP" udp dport 5353 counter accept comment "\"$TAG\""
        echo "opened port $PORT and mDNS for $HOST_IP in $fam $tab $chain"
    done

# Live firewall snapshot with counters, readable without root, for debugging connection trouble.
(while true; do nft -a list ruleset > "$DIR/ruleset-live.txt" 2>/dev/null; chown "${SUDO_USER:-root}" "$DIR/ruleset-live.txt"; sleep 3; done) &
DUMPER=$!

echo "relay running, Ctrl+C to stop"
"$BIN" relay --state "$STATE"
