FROM rust:1.95.0-bookworm AS builder
WORKDIR /app

COPY services/market-manager/Cargo.toml services/market-manager/Cargo.lock* ./
COPY services/market-manager/src ./src
COPY crates/chain /crates/chain
COPY crates/logging /crates/logging

RUN cargo build --release

FROM debian:bookworm-slim
WORKDIR /app

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/dodex-market-manager /usr/local/bin/dodex-market-manager
COPY services/market-manager/config ./config

ENV APP_CONFIG=/app/config/market-manager.stage.yaml

CMD ["dodex-market-manager"]
