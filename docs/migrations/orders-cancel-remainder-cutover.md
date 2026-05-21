# Orders Cancel-Remainder Cutover

`OrderBook.OrderCancelled` preserves the row's `amount_remaining` value so
`executedQty = amount_initial - amount_remaining` holds across cancellation.

If a data-bearing environment contains `CANCELLED` `live_orders` rows produced
by a projector that zeroed `amount_remaining`, clear and reproject the full
order lifecycle before exposing `/api/v1/orders`:

1. Set `raw_events.processed_at = NULL` for affected `OrderBook.OrderPlaced`,
   `OrderBook.OrderFilled`, and `OrderBook.OrderCancelled` rows.
2. Delete the affected `live_orders` rows.
3. Let `reproject_pending` replay the lifecycle through the current projector.

Replays must include the full lifecycle, not only the placement event, because
`OrderPlaced` can reopen a row on conflict before later fill/cancel events
restore the final status and executed quantity.
