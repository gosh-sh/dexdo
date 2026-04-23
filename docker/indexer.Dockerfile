FROM rust:1.87-bookworm AS builder
WORKDIR /app

COPY Cargo.toml ./
COPY crates ./crates
COPY services ./services

RUN cargo build --release -p dodex-indexer

FROM debian:bookworm-slim
WORKDIR /app

COPY --from=builder /app/target/release/dodex-indexer /usr/local/bin/dodex-indexer
COPY config ./config

ENV APP_CONFIG=/app/config/local.yaml

CMD ["dodex-indexer"]

