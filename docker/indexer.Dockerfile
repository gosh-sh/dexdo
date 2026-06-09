FROM rust:1.95.0-bookworm AS builder
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY services ./services
COPY contracts ./contracts
COPY migrations ./migrations

RUN cargo build --release -p dodex-indexer

FROM debian:bookworm-slim
WORKDIR /app

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/dodex-indexer /usr/local/bin/dodex-indexer
COPY config ./config

ENV APP_CONFIG=/app/config/indexer.local.yaml

CMD ["dodex-indexer"]
