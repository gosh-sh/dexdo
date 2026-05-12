# Changelog

All notable changes to DODEX are recorded here. Entries are date-based.

## [2026-05-12]

### Changed

- Restructured `docs/tech-specs/`:
  - `market-data-api.md` → `read-api.md` (scope: all read endpoints).
  - `market-data-indexer.md` → `indexer.md`.
  - `trading-api/write-api.md` → `write-api.md` (top-level).
- Updated cross-references in `services/api/README.md`, `services/indexer/README.md`, `AGENT_REQUIREMENTS.md`, and inline test comments.
- Rewrote root `README.md` for DODEX (previous content was carried over from a different project).

### Added

- `docs/README.md` — documentation map with file ownership.
- `CHANGELOG.md` (this file).

### Removed

- `docs/tech-specs/trading-api/` directory (empty `read-api.md` removed; `write-api.md` promoted to top-level).
- `internal-docs/` directory (was gitignored; local-only).
