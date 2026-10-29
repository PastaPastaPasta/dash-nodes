FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 1000 --create-home app && mkdir /data && chown app:app /data
COPY --from=build /src/target/release/dash-nodes /usr/local/bin/dash-nodes
COPY web /app/web
ENV WEB_DIR=/app/web DATA_DIR=/data
USER app
EXPOSE 8080
ENTRYPOINT ["dash-nodes"]
CMD ["serve"]
