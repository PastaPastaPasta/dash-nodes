# cjdns (https://github.com/cjdelisle/cjdns) built from an upstream release tag. The build needs
# Rust and Node.js (node_build compiles the C half); the runtime image needs jq for the
# entrypoint's config edits, iptables for the NAT that lets other containers use this node, and
# iproute2 (also used by the cjdns-route helper).
FROM rust:1-bookworm AS build
ARG CJDNS_REF=cjdns-v22.3
RUN apt-get update && apt-get install -y --no-install-recommends nodejs python3 git && rm -rf /var/lib/apt/lists/*
RUN git clone --depth 1 --branch "$CJDNS_REF" https://github.com/cjdelisle/cjdns /src
WORKDIR /src
# CARGO_BUILD_JOBS keeps the build from using every core (and its memory) on small hosts.
ARG CARGO_BUILD_JOBS=4
# Log_LEVEL is compiled in; the default (DEBUG) logs every ping.
RUN CARGO_BUILD_JOBS="$CARGO_BUILD_JOBS" Log_LEVEL=INFO NO_TEST=1 ./do

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends jq iproute2 iptables && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/cjdroute /src/cjdnstool /usr/local/bin/
COPY entrypoint.sh /usr/local/bin/cjdns-entrypoint.sh
VOLUME ["/etc/cjdns"]
ENTRYPOINT ["/bin/sh", "/usr/local/bin/cjdns-entrypoint.sh"]
