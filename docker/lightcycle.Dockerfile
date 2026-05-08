# syntax=docker/dockerfile:1.7

FROM rust:1.80-slim AS builder
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
EXPOSE 13042 9100
ENTRYPOINT ["/usr/local/bin/lightcycle"]
CMD ["stream"]
