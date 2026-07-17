# syntax=docker/dockerfile:1
# coturn — TURN/STUN server for Qubox NAT traversal.
#
# Build:
#   docker build -t qubox/coturn:latest -f ops/coturn/Dockerfile .
#
# Run:
#   docker run -d --name coturn \
#     -p 3478:3478/udp -p 3478:3478/tcp \
#     -p 5349:5349/udp -p 5349:5349/tcp \
#     qubox/coturn:latest

FROM ubuntu:24.04 AS build

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
        coturn \
        ca-certificates \
    ; \
    rm -rf /var/lib/apt/lists/*

FROM ubuntu:24.04

RUN set -eux; \
    groupadd --system coturn; \
    useradd --system --gid coturn --no-create-home --shell /usr/sbin/nologin coturn; \
    mkdir -p /var/lib/coturn /var/log/coturn; \
    chown coturn:coturn /var/lib/coturn /var/log/coturn

# copy coturn from the build stage
COPY --from=build /usr/bin/turnserver /usr/bin/turnserver
COPY --from=build /usr/bin/turnutils* /usr/bin/
COPY --from=build /usr/lib/*/libcrypto* /usr/lib/
COPY --from=build /usr/lib/*/libssl* /usr/lib/
COPY --from=build /usr/lib/*/libevent* /usr/lib/
COPY --from=build /usr/lib/*/libsqlite3* /usr/lib/
COPY --from=build /etc/turnserver.conf /etc/turnserver.conf

# override with our config
COPY ops/coturn/turnserver.conf /etc/turnserver.conf

RUN chown coturn:coturn /etc/turnserver.conf

EXPOSE 3478/tcp 3478/udp 5349/tcp 5349/udp

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD turnutils_stunclient -p 3478 localhost 2>/dev/null | grep -q "success"

USER coturn

ENTRYPOINT ["/usr/bin/turnserver", "-c", "/etc/turnserver.conf"]
