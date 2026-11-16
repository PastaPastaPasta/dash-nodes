#!/bin/sh
# Starts Tor with its control port locked by a password, for the dashds to create their onion
# services through (dashd -torcontrol). The password is made on the first start and kept in the
# tor-control volume, which only Tor and the dashds mount; dashd runs as another user, so it's
# world-readable there.
set -eu
PW=/var/lib/tor-control/password
if [ ! -s "$PW" ]; then
  head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$PW.tmp"
  chmod 0644 "$PW.tmp"
  mv -f "$PW.tmp" "$PW"
fi
HASH=$(tor --quiet --hash-password "$(cat "$PW")")
exec tor -f /etc/tor/torrc --ControlPort 0.0.0.0:9051 --HashedControlPassword "$HASH"
