#!/usr/bin/env bash
set -euo pipefail

# Implementation validation via Codex CLI
# Usage: scripts/validate-implementation.sh <agent-prompt> <review-rules> <diff-scope>
#
# Examples:
#   After tester: scripts/validate-implementation.sh "prompts/tester/func-...-tests.md" ".claude/roles/reviewer-review-tester.md" "tests/ crates/*/tests/ services/*/tests/"
#   After coder:  scripts/validate-implementation.sh "prompts/coder/func-...-code.md" ".claude/roles/reviewer-review-coder.md" "crates/ services/ migrations/ ':!**/tests/' ':!tests/'"
#
# Exit codes:
# 0 = APPROVED
# 1 = CONCERNS
# 2 = error

AGENT_PROMPT="${1:?Usage: validate-implementation.sh <agent-prompt> <review-rules> <diff-scope> [base-ref]}"
REVIEW_RULES="${2:?Missing review rules path}"
DIFF_SCOPE="${3:?Missing diff scope (e.g. 'test/' or ':!test/')}"
BASE_REF="${4:-dev}"
TARGET_REF="${5:-HEAD}"
VALIDATOR_MODEL="${VALIDATOR_MODEL:-gpt-5.4}"
VALIDATOR_REASONING="${VALIDATOR_REASONING:-medium}"

command -v codex >/dev/null 2>&1 || {
  echo "ERROR: codex CLI is required" >&2
  exit 2
}

PROMPT_TEXT="You are validating that an implementation matches its prompt and does not leave correctness bugs in the PR.

First read these control files:
- Agent prompt: ${AGENT_PROMPT}
- Review rules: ${REVIEW_RULES}
- Core reviewer rules: .claude/roles/reviewer.md
- Repository-change rules: AGENT_REQUIREMENTS.md
- Documentation map: docs/README.md (file ownership and which spec applies to which area)
- Public functional contract: docs/api-spec.md (sacred — implementation must not contradict it; changes to api-spec.md require explicit owner approval)
- Implementation specs applicable to the task scope: docs/tech-specs/read-api.md, write-api.md, indexer.md, auth.md, data-schema.md, and docs/contract-specs/ for on-chain semantics.

Start with this command to inspect the implementation:
git diff ${BASE_REF}..${TARGET_REF} -- ${DIFF_SCOPE}

Then run a discovery pass before deciding:
1. Run: git diff --name-only ${BASE_REF}..${TARGET_REF}
2. From the prompt and diff, extract changed/removed/renamed identifiers, DB columns, config keys (config/*.yaml), SQL table/column names, enum variants, trait methods, handlers, and test helpers.
3. For each extracted term that could have residual call sites, run targeted rg searches across the relevant roots, not only changed files. Typical roots: crates services tests migrations docs config Cargo.toml CHANGELOG.md.
4. For model-change or removal steps, residual search is mandatory. Check every hit and classify it as expected history (migrations/0001_*.sql etc.) / docs / migration / test coverage or a real leftover.
5. If the prompt lists affected files, verify the list is complete enough by searching for the same pattern in neighboring modules/tests. Unlisted affected call sites are in scope when they would break runtime, schema, tests, rollback, or accepted behavior.

Then run a behavioral invariant pass before deciding. This is mandatory for any change that touches persistent state, state machines, background jobs, scheduled ticks, retries, recovery/repair/cleanup logic, diagnostics, delivery/idempotency, provider dispatch, or migrations:
1. Identify the source of truth, any derived/cached/copied data, allowed lifecycle states, allowed transitions, uniqueness/idempotency guards, required relationships, and expected diagnostics.
2. For each new or changed branch, test it mentally against non-happy persisted shapes, not only the normal setup from new tests: missing related row, stale derived row, duplicate row, orphan row, terminal row, intermediate row, legacy/null value, partially migrated data, and data left by an older version.
3. Check operation ordering. If code mutates storage before validation, transition, external call, or another dependent update, consider failure after the first mutation: throw, null, duplicate, retry, restart, or next tick.
4. Check swallowed errors and best-effort repair paths. If an error is caught, verify the code does not log/report success while leaving data in a state that will be reprocessed forever, hidden from diagnostics, or blocked from future progress.
5. Every dirty data shape affected by the diff must be explicitly handled, explicitly ignored by a safe predicate, or explicitly diagnosable. If it is just invisible, silently skipped, or converted into a misleading healthy/success state, return CONCERNS.

You may read files outside the diff when discovery shows they are affected by the same behavior, schema, helper, config field, or test fixture. Do not wander for style or unrelated cleanup.

Validate correctness-level issues. Return CONCERNS if one of these is true:
1. The implementation does not match the prompt in a way that changes real behavior.
2. The implementation introduces a real production bug or regression.
3. The implementation leaves an obviously affected call site unhandled for the same bug.
4. The implementation violates an explicit review rule in a way that matters for correctness.
5. The implementation changes accepted behavior/tests without a confirmed spec change.
6. The implementation leaves stale tests, fixtures, SQL, docs contracts, config contracts, or migration/rollback paths that will fail after the changed model/schema/API is applied.
7. Required test coverage is missing in a way that would allow the changed behavior to regress unnoticed.
8. A changed stateful/background/recovery path can leave persistent data inconsistent, invisible to diagnostics, repeatedly reprocessed, or falsely reported as successfully handled.

Ignore:
- style preferences
- optional refactors
- non-blocking cleanup that cannot change behavior or test results
- theoretical edge cases
- ways the implementation could be “more complete” outside the current substep
- cosmetics, wording, naming, changelog phrasing, or docs polish unless they contradict a runtime/API/schema contract

If reviewing tester output:
- focus on whether the tests actually verify the intended behavior
- do not require extra tests unless the missing case would allow a real regression through
- for schema/model removal prompts, search existing tests for old fixtures and old expectations across tests/, crates/*/tests/, services/*/tests/, and inline #[cfg(test)] modules in changed source files; if unlisted tests will fail or preserve forbidden behavior, return CONCERNS

If reviewing coder output:
- focus on whether the code really implements the prompt and preserves expected behavior
- do not require unrelated cleanup or broader rewrites
- for DB migrations, check the new migration file, idempotency via if not exists / if exists, data preservation across the forward migration, indexes/constraints/FK implications, the inline #[cfg(test)] tests and per-crate sqlx::test integration tests that run all migrations, and the read-side query/projection consequences in services/api and services/indexer
- for config/API/filter changes, check both struct construction and all call sites; absence of a key in config/*.yaml is different from a Some(None) Option when the prompt says so

Respond with EXACTLY one line:
APPROVED
or
CONCERNS: <short explanation>"

FULL_OUTPUT="$(codex exec -m "$VALIDATOR_MODEL" -c "model_reasoning_effort=\"$VALIDATOR_REASONING\"" "$PROMPT_TEXT" --full-auto 2>&1)" || {
  echo "ERROR: codex failed" >&2
  echo "$FULL_OUTPUT" >&2
  exit 2
}

RESULT="$(echo "$FULL_OUTPUT" | grep -E '^(APPROVED|CONCERNS:)' | tail -1)"

mkdir -p prompts/context
echo "$FULL_OUTPUT" > prompts/context/last-implementation-review.txt

if [[ -z "$RESULT" ]]; then
  echo "$FULL_OUTPUT"
  echo "WARNING: unexpected response format — no APPROVED/CONCERNS line found" >&2
  exit 1
fi

echo "$RESULT"

if [[ "$RESULT" == "APPROVED" ]]; then
  exit 0
else
  exit 1
fi
