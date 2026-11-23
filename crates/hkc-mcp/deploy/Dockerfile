# hkc-mcp container image.
#
# Build from the repository root:
#   docker build -f crates/hkc-mcp/deploy/Dockerfile -t hkc-mcp .
#
# Data (login session, OAuth state, audit log) lives in /data: mount a volume.
# Configuration comes from environment variables (see docs/mcp.md); pass them
# with --env-file. Docker's --env-file takes values literally.

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
ENV HKC_MCP_DATA_DIR=/data \
    HKC_MCP_LISTEN=0.0.0.0:8787
VOLUME ["/data"]
EXPOSE 8787
ENTRYPOINT ["hkc-mcp"]
CMD ["serve"]
