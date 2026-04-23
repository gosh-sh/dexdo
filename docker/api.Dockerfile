FROM rust:1.87-bookworm AS builder
WORKDIR /app

COPY Cargo.toml ./
COPY crates ./crates
COPY services ./services

RUN cargo build --release -p dodex-api

FROM debian:bookworm-slim
WORKDIR /app

COPY --from=builder /app/target/release/dodex-api /usr/local/bin/dodex-api
COPY config ./config

ENV APP_CONFIG=/app/config/local.yaml

CMD ["dodex-api"]

