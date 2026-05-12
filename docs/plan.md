# DODEX Implementation Plan

Active multi-step implementation plan, organised by development area. Each section is owned by one developer; cross-area work is captured by referencing both sections.

## How to use

- One section per development area. Numbering is local to the section — there is no global step counter.
- Each step has a stable label (e.g. `Step 1: ...`) and a status (`Planned` / `In progress` / `Done`).
- Steps reference the tech-spec section they implement.

## Dev1 — Trading write API

Scope: `POST /api/v1/order`, `DELETE /api/v1/order`, `POST /api/v1/batchOrders`, `DELETE /api/v1/batchOrders`, `DELETE /api/v1/openOrders`. Tech spec: [tech-specs/write-api.md](tech-specs/write-api.md).

_No steps planned yet._

## Dev2 — Read API and indexer

Scope: `GET /api/v1/markets`, `GET /api/v1/depth`, `GET /api/v1/account`, `GET /api/v1/openOrders`, `GET /api/v1/allOrders`, indexer (`services/indexer`). Tech specs: [tech-specs/read-api.md](tech-specs/read-api.md), [tech-specs/indexer.md](tech-specs/indexer.md).

_No steps planned yet._
