FROM rust:1.95.0-bookworm AS builder
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY services ./services
COPY contracts ./contracts
COPY migrations ./migrations

RUN cargo build --release -p dodex-api

FROM debian:bookworm-slim
WORKDIR /app

# `curl` is needed by the compose healthcheck to probe `/readiness`.
RUN apt-get update \
 && apt-get install -y --no-install-recommends curl ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/dodex-api /usr/local/bin/dodex-api
COPY config ./config

ENV APP_CONFIG=/app/config/api.local.yaml

CMD ["dodex-api"]
