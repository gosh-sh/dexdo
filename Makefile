# Developer shortcuts. `make check` is the canonical local verification and
# mirrors CI (.github/workflows/pr-tests.yml): fmt + clippy + full test suite.

COMPOSE_TEST := docker compose -f docker-compose.test.yml

.PHONY: check fmt fmt-check clippy test test-db-up test-db-down

check: fmt-check clippy test

fmt:
	cargo +nightly fmt --all

fmt-check:
	cargo +nightly fmt --all -- --check

clippy:
	cargo clippy --workspace --all-targets --no-deps -- -D warnings

# Full suite needs the test Postgres (brought up automatically). Uses nextest
# when installed (matches CI), plain cargo test otherwise; doc tests either way.
test: test-db-up
	@if command -v cargo-nextest >/dev/null 2>&1; then \
		cargo nextest run --workspace --no-fail-fast; \
	else \
		cargo test --workspace; \
	fi
	cargo test --workspace --doc

# Bring up the throw-away test Postgres (see README.md#test-postgres) and make
# sure .env exists — fresh worktrees don't inherit it, and the DB-gated tests
# read TEST_DATABASE_URL from .env via dotenvy.
test-db-up:
	@test -f .env || cp .env.example .env
	@pg_isready -h localhost -p 55432 -q 2>/dev/null || $(COMPOSE_TEST) up -d --wait

test-db-down:
	$(COMPOSE_TEST) down -v
