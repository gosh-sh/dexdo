# Project Documentation Rules

The public REST API contract is [`docs/api-spec.md`](docs/api-spec.md); do not edit it unless the task explicitly asks to change the public API.

## Before every `git commit`

Re-read **every** file under `docs/` and the `README.md` of every touched component, then update each one the staged diff invalidates. Default is "check all"; only skip a doc after re-reading it and confirming it is unaffected.

This includes terminology renames (e.g. `OEL` → `OracleEventList`) and schema/field shape changes — propagate them across all docs, not just the file whose name matches the code change.

If a doc is now obsolete and has no salvageable content, delete it and remove references in the same commit.
