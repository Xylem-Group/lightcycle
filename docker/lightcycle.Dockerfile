# syntax=docker/dockerfile:1.7
#
# Build:  docker build -f docker/lightcycle.Dockerfile -t lightcycle:dev .
# Run:    docker run --rm -p 9528:9528 lightcycle:dev stream \
#                    --rpc-url http://host.docker.internal:8090
#
# The kulen module (modules/lightcycle.nix in the kulen repo) deploys
# this image via virtualisation.oci-containers, mirroring the java-tron
# module's pattern.

# Tracks rust-toolchain.toml in the workspace root. Bump together.
FROM rust:1.95-slim AS builder
WORKDIR /build

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev protobuf-compiler ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --bin lightcycle && \
    cp target/release/lightcycle /usr/local/bin/lightcycle

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 1000 -m lightcycle

COPY --from=builder /usr/local/bin/lightcycle /usr/local/bin/lightcycle

USER lightcycle
# 13042 = future Firehose gRPC server (sf.firehose.v2.Stream).
# 9528  = Prometheus /metrics endpoint. 9527 is taken by java-tron's
#         exporter; bumping by one keeps both scrapeable in tandem
#         when both run on the same host.
EXPOSE 13042 9528
ENTRYPOINT ["/usr/local/bin/lightcycle"]
CMD ["stream"]
