# DODEX Documentation Map

## Functional contract

- [api-spec.md](api-spec.md) — public REST API contract. **Single source of truth for HTTP behavior. Do not change without explicit approval from the API contract owner.**

## Implementation specs

| File | Scope | Owner |
| --- | --- | --- |
| [tech-specs/read-api.md](tech-specs/read-api.md) | All read endpoints: `GET /markets`, `/depth`, `/account`, `/openOrders`, `/allOrders` | Dev2 |
| [tech-specs/write-api.md](tech-specs/write-api.md) | All write endpoints: `POST/DELETE /order`, `POST/DELETE /batchOrders`, `DELETE /openOrders` | Dev1 |
| [tech-specs/indexer.md](tech-specs/indexer.md) | Chain-event ingestion, projectors, reconcilers (`services/indexer`) | Dev2 |
| [tech-specs/auth.md](tech-specs/auth.md) | Authentication, authorization, account/api-key lifecycle | shared |
| [tech-specs/data-schema.md](tech-specs/data-schema.md) | Postgres tables, indexes, migrations | shared |

## Contracts

- [contract-specs/](contract-specs/) — on-chain DODEX contracts. Event routing in [dex-events-routing.md](contract-specs/dex-events-routing.md); flow/object diagrams as HTML and drawio.

## Conventions

- **One logical component → one tech-spec file.** Read API, write API, and indexer are each one file. Shared files (`data-schema.md`, `auth.md`) carry contributions from both developers; resolve conflicts on a per-section basis.
- All tech-specs reference [api-spec.md](api-spec.md) for HTTP shape; none duplicates it.
- Pre-commit doc-sweep is mandatory — see [../AGENT_REQUIREMENTS.md](../AGENT_REQUIREMENTS.md).
