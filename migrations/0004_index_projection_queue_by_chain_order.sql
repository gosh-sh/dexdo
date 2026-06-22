-- 0004: index the projection queue by chain_order.
--
-- The projection loop is the sole projector and reads every pending raw_events
-- row ordered by chain_order via a keyset cursor (chain_order > $after). The
-- existing pending partial index raw_events_pending_projection_idx is keyed by
-- (created_at_chain, id) -- it matches the WHERE predicate but not the ORDER BY,
-- so a large pending set is either sorted in full or reached by scanning
-- projected history through the non-partial raw_events_chain_order_idx. Key the
-- partial index by chain_order so the keyset scan is a forward range scan over
-- only pending rows. No query orders the pending set by created_at_chain
-- (verified across the repo), so the old partial index is dropped -- it would
-- only add write amplification on the capture insert path.
create index raw_events_pending_chain_order_idx
    on raw_events (chain_order)
    where processed_at is null
      and event_type is not null
      and decoded is not null;

drop index raw_events_pending_projection_idx;
