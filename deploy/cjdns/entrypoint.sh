#!/bin/sh
# cjdns node for dash-nodes. The node's identity (keys, IPv6 address) lives in
# /etc/cjdns/cjdroute.conf, generated on first start and kept on the volume. Every start rebuilds
# the running config from it: outbound UDP peers from $CJDNS_PEERS (the public hyperboria list),
# no Ethernet autopeering and no LAN beacons (they would broadcast for peers on the local network),
# the admin API on localhost only, a TUN device named cjdns0, logs on stdout. Nothing listens for
# inbound peers beyond the UDP socket cjdns needs for its outbound sessions; behind NAT that port
# isn't reachable from outside anyway. Public peers are also found through cjdns' DNS seeds.
set -eu

CONF=/etc/cjdns/cjdroute.conf
PEERS="${CJDNS_PEERS:-/etc/cjdns-peers/peers.json}"
RUN=/tmp/cjdroute.conf

if [ ! -s "$CONF" ]; then
  umask 077
  cjdroute --genconf | cjdroute --cleanconf > "$CONF.new"
  mv "$CONF.new" "$CONF"
  echo "cjdns: generated a new identity in $CONF"
fi

peers='{}'
if [ -s "$PEERS" ]; then
  peers="$(cat "$PEERS")"
fi

umask 077
jq --argjson peers "$peers" '
  .noBackground = 1
  | .admin.bind = "127.0.0.1:11234"
  | .interfaces = {UDPInterface: [(.interfaces.UDPInterface[0]
        | .connectTo = $peers | .beacon = 0 | del(.beaconDevices, .beaconPort))]}
  | .router.interface = {type: "TUNInterface", tunDevice: "cjdns0"}
  | .logging = {logTo: "stdout"}
' "$CONF" > "$RUN"

# CJDNS_NAT=1: route for other containers. Traffic they send to fc00::/8 (via this container, see
# cjdns-route in docker-compose.yml) leaves cjdns0 with this node's address; cjdns only carries
# packets from its own address.
if [ "${CJDNS_NAT:-0}" = 1 ]; then
  ip6tables -t nat -C POSTROUTING -o cjdns0 -j MASQUERADE 2>/dev/null \
    || ip6tables -t nat -A POSTROUTING -o cjdns0 -j MASQUERADE
fi

echo "cjdns: address $(jq -r .ipv6 "$CONF"), $(echo "$peers" | jq length) configured peers"
exec cjdroute --nobg < "$RUN"
