-- 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
--
-- TokenContract gained a claim path: the seller calls `claimTokens` with a
-- cumulative, never-decreasing figure and the contract reports TicksClaimed
-- (trusted, claimed).
--
-- This does NOT replace TickFinalized, which still fires from `_settleBoundaries`
-- and still writes an `inference_ticks` row per weekly boundary. The two answer
-- different questions: `inference_ticks` is the settled record at each boundary,
-- these two columns are the running position between boundaries. A subscription
-- week is 604800 s, so without them a deal shows no movement for days.
--
-- Both are high-water marks, mirroring the chain's own monotonicity
-- (`require(cumulativeTokens >= _tokensPend2)`), so a replayed or out-of-order
-- event cannot walk them backwards.

alter table inference_deals
    add column trusted_ticks numeric(78, 0),
    add column claimed_ticks numeric(78, 0);

comment on column inference_deals.trusted_ticks is
    'High-water mark of TicksClaimed.trusted: work the deal has actually credited.';
comment on column inference_deals.claimed_ticks is
    'High-water mark of TicksClaimed.claimed: what the seller has claimed but that '
    'has not yet been trusted or contested. Ahead of trusted_ticks while a claim is pending.';
