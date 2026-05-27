# Build context for this image is `./services/market-manager` (set in
# docker-compose.yml) — that's deliberate, so the root workspace and
# other services cannot leak into the build. Staging-only tool.
#
# All chain deps are public HTTPS (ackinacki-kit); no SSH forwarding
# needed.

FROM rust:1.95.0-bookworm AS builder
WORKDIR /app

COPY Cargo.toml Cargo.lock* ./
COPY src ./src

RUN cargo build --release

FROM debian:bookworm-slim
WORKDIR /app

# ca-certificates: HTTPS to shellnet GraphQL/giver endpoints.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/dodex-market-manager /usr/local/bin/dodex-market-manager
COPY config ./config

ENV APP_CONFIG=/app/config/market-manager.stage.yaml

CMD ["dodex-market-manager"]
