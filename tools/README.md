# tools/

End-user **command-line tools** — one-shot programs a person runs to get something
done (onboard a wallet, mint a pool, deploy an oracle). Not long-running daemons
(those are [`services/`](../services/)) and not reusable libraries (those are
[`crates/`](../crates/) and [`sdk/`](../sdk/)).

See [`../CONTRIBUTING.md`](../CONTRIBUTING.md) for the full "where does my code go?"
decision guide; this README is just the local entry point.

## Why this is its own workspace

`tools/` is a **separate Cargo workspace**, excluded from the root
`cargo build --workspace` — the same reason [`sdk/`](../sdk/) is. Most user tools
reach for the SDK's halo2/zk dependency graph, and that heavy graph must stay out
of the root build. Tools depend on the libs **by path** (`../sdk`, `../crates/*`).

> A tool that needs *only* the lightweight root crates (no `sdk`, no halo2) may
> instead be a `[[bin]]` in the root workspace — the rule keys off the dependency
> graph, not dogma. When in doubt, put it here.

## Adding a tool

Each tool is its own crate under `tools/<tool-name>/`. Name the binary
`<verb>_<noun>` (e.g. `onboard_user`, `mint_pool`), matching the existing
convention in [`sdk/src/bin/`](../sdk/src/bin/).

1. Create `tools/<tool-name>/` with a `Cargo.toml` and `src/main.rs`.
2. Register it in the workspace. **The first tool also creates `tools/Cargo.toml`:**

   ```toml
   # tools/Cargo.toml
   [workspace]
   members = ["<tool-name>"]
   resolver = "3"
   ```

   Subsequent tools just add their directory to `members`.
3. Depend on the libs by path, e.g.:

   ```toml
   # tools/<tool-name>/Cargo.toml
   [dependencies]
   dodex-sdk = { path = "../../sdk" }
   dodex-domain = { path = "../../crates/domain" }
   ```
4. Give the tool a top-of-file doc comment and a `--help`. If it writes user
   secrets or submits transactions, follow the autonomy/secrecy conventions the
   onboarding tooling already uses (show every command, never log secrets).

## Reusable logic does not live here

If a second tool — or a service — would copy code out of a tool, that code belongs
in a lib (`crates/*` or `sdk/`), and the tool calls it. Tools are thin wiring over
the libraries; see [`../CONTRIBUTING.md`](../CONTRIBUTING.md#where-code-goes).

## Grandfathered tools

The end-user binaries still in [`sdk/src/bin/`](../sdk/src/bin/) —
`onboard_user_shellnet`, `mint_pn_pool`, `mint_ob_pool`, `deploy_oracle` — predate
this folder. They keep working where they are and migrate here only when next
touched; don't move them in an unrelated change.
