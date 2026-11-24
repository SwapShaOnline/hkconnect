# hkc-mcp container image.
#
# Build from the repository root:
#   docker build -f crates/hkc-mcp/deploy/Dockerfile -t hkc-mcp .
#
# Data (login session, OAuth state, audit log) lives in /data: mount a volume
# there (`-v hkc-mcp-data:/data`). There is deliberately no VOLUME line:
# Railway refuses Dockerfiles that have one.
# Configuration comes from environment variables (see docs/mcp.md); pass them
# with --env-file. Docker's --env-file takes values literally.
#
# Prebuilt multi-arch images: ghcr.io/swapshaonline/hkc-mcp (see
# docs/railway.md for running it on Railway, where PORT, the public URL and
# the proxy are picked up automatically).

FROM rust:1.89-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p hkc-mcp

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data --shell /usr/sbin/nologin hkc \
    && mkdir -p /data && chown hkc /data && chmod 700 /data
COPY --from=build /src/target/release/hkc-mcp /usr/local/bin/hkc-mcp
USER hkc
# The server must listen on all interfaces *inside* the container to be
# reachable at all. Publish the port on the host's loopback only
# (`-p 127.0.0.1:8787:8787`) and put the tunnel or proxy in front; never
# `-p 8787:8787`, which exposes plain HTTP to the network.
#
# X-Forwarded-For is believed only from a loopback peer unless
# HKC_MCP_TRUSTED_PROXIES lists the proxy's address. Seen from inside the
# container, a proxy on the host connects from the Docker bridge (e.g.
# 172.17.0.1), so set HKC_MCP_TRUSTED_PROXIES to that address (see
# docs/mcp.md) rather than trusting everyone.
#
# On Railway, $PORT wins over HKC_MCP_LISTEN (see docs/railway.md).
ENV HKC_MCP_DATA_DIR=/data \
    HKC_MCP_LISTEN=0.0.0.0:8787
EXPOSE 8787
ENTRYPOINT ["hkc-mcp"]
CMD ["serve"]
