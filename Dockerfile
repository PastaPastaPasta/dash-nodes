FROM rust:1-bookworm AS build
# rs-x11-hash (X11 block hashes for the bench) compiles its C code with clang.
RUN apt-get update && apt-get install -y --no-install-recommends clang && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 1000 --create-home app && mkdir /data /state && chown app:app /data /state
COPY --from=build /src/target/release/dash-nodes /usr/local/bin/dash-nodes
COPY web /app/web
ENV WEB_DIR=/app/web DATA_DIR=/data STATE_DIR=/state
USER app
EXPOSE 8080
ENTRYPOINT ["dash-nodes"]
CMD ["serve"]
