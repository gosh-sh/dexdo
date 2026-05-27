#!/usr/bin/env bash
# Regenerate docs/openapi.yaml from the Rust handler annotations in
# services/api and validate the result with @redocly/cli. Run from
# anywhere; the script resolves the repo root from its own location.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
OUT="${REPO_ROOT}/docs/openapi.yaml"

cd "${REPO_ROOT}"
cargo run --quiet -p dodex-api --bin gen-openapi -- --out "${OUT}"
npx -y @redocly/cli@latest lint "${OUT}"
echo "openapi.yaml regenerated and linted"
