#!/usr/bin/env bash
set -euo pipefail

# Prompt validation via Codex CLI
# Usage: scripts/validate-prompts.sh <tester-prompt|none> <coder-prompt|none> "<task description>"
#
# Exit codes:
# 0 = APPROVED
# 1 = CONCERNS
# 2 = error

TESTER_PROMPT="${1:?Usage: validate-prompts.sh <tester-prompt|none> <coder-prompt|none> \"<description>\"}"
CODER_PROMPT="${2:?Missing coder prompt path (or 'none')}"
TASK_DESC="${3:?Missing task description}"

command -v codex >/dev/null 2>&1 || {
  echo "ERROR: codex CLI is required" >&2
  exit 2
}

PROMPT_TEXT="You are validating development prompts before implementation. The output remains a binary gate, but APPROVED must mean the prompts are complete enough to prevent correctness bugs.

First read these control files:
- Tester prompt: ${TESTER_PROMPT}
- Coder prompt: ${CODER_PROMPT}
- Prompt rules: .claude/roles/reviewer-prompts-tester.md and .claude/roles/reviewer-prompts-coder.md
- Specs (read ALL of these):
  - docs/product-spec.md (product behavior and requirements)
  - docs/worker/technical-spec.md (architecture, invariants)
  - docs/worker/conversation-lifecycle.md (status conversation runtime)
  - docs/worker/coordination-flows.md (reports, pipeline, review flows)
  - docs/worker/db-schema.md (database schema reference)
  - docs/domain-glossary.md (business vocabulary)
  - docs/naming-conventions.md (code naming rules)
  - docs/worker/plan.md (active multi-step plan; if a step is in progress, validate prompts against the matching Step section: scope, forbidden residual patterns, naming invariants, acceptance criteria)

Then run a discovery pass before deciding:
1. Read the apps/worker/src/ or test/ files explicitly mentioned in those prompts.
2. From the task description, specs, and prompts, extract changed/removed/renamed identifiers, DB columns, config keys, SQL table/column names, enum/state names, handlers, and test helpers.
3. For each extracted term that could have residual call sites, run targeted rg searches across relevant roots, not only mentioned files. Typical roots: apps/worker/src test db docs config.example.json package.json CHANGELOG.md.
4. For model-change or removal steps, residual search is mandatory. Verify the prompts name all affected runtime call sites, test fixtures, existing tests that encode removed behavior, migration/rollback paths, and docs/contracts that must change.
5. If a discovered affected file is not mentioned in the prompt, decide whether omission would cause a real runtime/test/schema/rollback regression. If yes, return CONCERNS.

Then run a behavioral invariant pass before deciding. This is mandatory when the task touches persistent state, state machines, background jobs, scheduled ticks, retries, recovery/repair/cleanup logic, diagnostics, delivery/idempotency, provider dispatch, or migrations:
1. Identify the source of truth, any derived/cached/copied data, allowed lifecycle states, allowed transitions, uniqueness/idempotency guards, required relationships, and expected diagnostics.
2. Verify that the tester prompt covers the important non-happy persisted shapes for this task: missing related row, stale derived row, duplicate row, orphan row, terminal row, intermediate row, legacy/null value, partially migrated data, and data left by an older version.
3. Verify that the coder prompt tells the implementer how to handle those shapes: handle, safely ignore by explicit predicate, or diagnose. It is not enough to describe only the normal path.
4. If the proposed implementation will mutate storage before validation, transition, external call, or another dependent update, verify the prompts include failure-ordering expectations: throw, null, duplicate, retry, restart, or next tick after partial success.
5. If cleanup/repair/diagnostics code catches errors, verify the prompts forbid reporting success while data remains inconsistent, invisible to diagnostics, repeatedly reprocessed, or blocked from future progress.

Do not explore for style or unrelated cleanup.
Do not search for the latest context file.
Use this task description as the owner context: ${TASK_DESC}

Validate correctness-level issues. Return CONCERNS if one of these is true:
1. The prompt scope leaks into another refactor substep.
2. The prompt contradicts any of the spec documents listed above.
3. The prompt asks for a code/test change that is clearly impossible or wrong for the current code.
4. The prompt misses another clearly affected call site for the same bug, and that miss would cause a real production regression.
5. The prompt tells the tester to rewrite accepted tests without a confirmed spec change.
6. For model-change steps (where the system switches its source of truth): the coder prompt fails to explicitly list forbidden residual patterns that must be absent after the step, or the tester prompt fails to list existing tests that encode now-forbidden behavior and must be deleted/rewritten.
7. The tester prompt lists a test for deletion but the coder prompt removes the code path that test relied on — and there is ANOTHER test (not listed for deletion) that also relies on the same removed code path. That unlisted test will break.
8. The prompt misses stale fixtures, SQL, config examples, docs contracts, migrations, down migrations, or all-migrations tests that are affected by the changed schema/model/API.
9. The proposed tests can pass while the core requested behavior is still broken.
10. For a stateful/background/recovery task, the prompts only cover the happy path and do not specify what happens to dirty persisted data, partial updates, swallowed errors, or diagnosable failure states.

Ignore:
- style preferences
- optional improvements
- theoretical edge cases
- requests to make the prompt more complete than needed for this substep
- cosmetics, wording, naming, changelog phrasing, or docs polish unless they contradict a runtime/API/schema contract

Respond with EXACTLY one line:
APPROVED
or
CONCERNS: <short explanation>"

FULL_OUTPUT="$(codex exec -m gpt-5.4 -c 'model_reasoning_effort="medium"' "$PROMPT_TEXT" --full-auto 2>&1)" || {
  echo "ERROR: codex failed" >&2
  echo "$FULL_OUTPUT" >&2
  exit 2
}

RESULT="$(echo "$FULL_OUTPUT" | grep -E '^(APPROVED|CONCERNS:)' | tail -1)"

mkdir -p prompts/context
echo "$FULL_OUTPUT" > prompts/context/last-validation-result.txt

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
