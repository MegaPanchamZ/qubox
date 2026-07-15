# syntax=docker/dockerfile:1
# qubox-signaling-server — WebSocket signaling + TURN credential endpoint.
#
# Build (from repo root):
#   docker build -t qubox/signaling-server:latest \
#     -f apps/qubox-signaling-server/Dockerfile .
#
# Run (self-host; signed Hello required by default):
#   docker run -d --name qubox-signaling \
#     -p 7000:7000 \
#     -e QUBOX_SIGNALING_SECRET="$(openssl rand -hex 32)" \
#     -e QUBOX_PAIRING_STORE=/data/pairings.json \
#     -v qubox-data:/data \
#     qubox/signaling-server:latest

FROM rust:1.85-slim-bookworm AS builder

RUN apt-get update && \
    apt-get install -y --no-install-recommends pkg-config libssl-dev && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .
RUN cargo build --release -p qubox-signaling-server && \
    cp target/release/qubox-signaling-server /qubox-signaling-server

FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/* && \
    groupadd --system qubox && \
    useradd --system --gid qubox --no-create-home --shell /usr/sbin/nologin qubox && \
    mkdir -p /data && chown qubox:qubox /data

COPY --from=builder /qubox-signaling-server /usr/local/bin/qubox-signaling-server
RUN chown qubox:qubox /usr/local/bin/qubox-signaling-server

ENV QUBOX_BIND=0.0.0.0:7000 \
    QUBOX_PAIRING_STORE=/data/pairings.json \
    RUST_LOG=info

EXPOSE 7000/tcp
VOLUME ["/data"]
USER qubox

HEALTHCHECK --interval=15s --timeout=3s --start-period=10s --retries=3 \
    CMD ["sh", "-c", "exec 3<>/dev/tcp/127.0.0.1/7000"]

ENTRYPOINT ["/usr/local/bin/qubox-signaling-server"]
