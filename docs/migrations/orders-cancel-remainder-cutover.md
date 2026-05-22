# Orders Read-Model Cutover

This document covers the wipe-and-reproject procedure for `live_orders`
data produced by older projector logic. Two concrete instances motivate
it today:

- **Cancel-remainder.** `OrderBook.OrderCancelled` now preserves the
  row's `amount_remaining` so `executedQty = amount_initial - amount_remaining`
  holds across cancellation. Older projector output zeroed
  `amount_remaining` on cancel, breaking `executedQty` for partially
  filled cancels.
- **Mutated-row ON CONFLICT guard.** `apply_order_placed` now
  `WHERE`-guards its conflict arm to fire only on a row that is still
  in its fresh, unmutated state (status non-terminal AND
  `amount_remaining = amount_initial`). An isolated `OrderPlaced`
  replay against a row that is `FILLED` / `CANCELLED` / `REJECTED`,
  or against a partial-fill OPEN row, is dropped instead of overwriting
  the mutated state. Older projector output would silently reopen a
  terminal row or zero out fill history on a partial-fill row when
  only the placement event was replayed.

Either condition means historical `live_orders` rows may not match the
shape the current projector emits. Clear and reproject the full order
lifecycle before exposing `/api/v1/orders`:

1. Set `raw_events.processed_at = NULL` for affected `OrderBook.OrderPlaced`,
   `OrderBook.OrderFilled`, and `OrderBook.OrderCancelled` rows.
2. Delete the affected `live_orders` rows.
3. Let `reproject_pending` replay the lifecycle through the current projector.

Replays must include the full lifecycle, not only the placement event. Step
2's `delete` is what enables the `OrderPlaced` insert to land cleanly: the
projector's `ON CONFLICT` arm only fires on a row in its fresh, unmutated
state, so an `OrderPlaced` replay against a terminal row OR a partial-fill
OPEN row is dropped rather than overwriting the mutated state. A skipped
replay is logged at `warn!` with
`"OrderPlaced replay refused on mutated row (terminal status or partial
fill); partial-replay cutover suspected"` — operators should expect to see
that line zero times in a correct wipe-and-reproject; a non-zero count
means step 2 missed rows.
