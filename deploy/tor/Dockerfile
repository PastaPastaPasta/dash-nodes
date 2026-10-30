# Tor client only (SOCKS for reaching .onion peers): no relay, no exit, no onion service.
# Debian's package; trixie carries the current 0.4.8 series with security updates.
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends tor && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /var/lib/tor && chown debian-tor:debian-tor /var/lib/tor && chmod 0700 /var/lib/tor
COPY torrc /etc/tor/torrc
USER debian-tor
VOLUME ["/var/lib/tor"]
ENTRYPOINT ["tor", "-f", "/etc/tor/torrc"]
