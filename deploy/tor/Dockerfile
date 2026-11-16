# Tor client (SOCKS for reaching .onion peers) whose control port gives the dashds their onion
# services: no relay, no exit. Debian's package; trixie carries the current 0.4.8 series with
# security updates.
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends tor && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /var/lib/tor /var/lib/tor-control \
    && chown debian-tor:debian-tor /var/lib/tor /var/lib/tor-control \
    && chmod 0700 /var/lib/tor && chmod 0755 /var/lib/tor-control
COPY torrc /etc/tor/torrc
COPY entrypoint.sh /usr/local/bin/tor-entrypoint
USER debian-tor
VOLUME ["/var/lib/tor", "/var/lib/tor-control"]
ENTRYPOINT ["/bin/sh", "/usr/local/bin/tor-entrypoint"]
