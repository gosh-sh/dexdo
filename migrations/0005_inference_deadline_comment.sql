-- 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
--
-- The 0002 comment forbade deriving a status from `deadline` and promised that an
-- order stays OPEN until `InferenceOrderExpired`. The second half is wrong: a
-- continuation resumed past its deadline (`InferenceOrderBook.sol:1355-1372`) emits
-- ONLY `InferenceRefunded` — there is no expiry event on that path at all.
--
-- The first prohibition still stands, stated more precisely: comparing the deadline
-- to the CLOCK is still forbidden, since elapsed time by itself closes nothing. The
-- comparison is allowed in exactly one place — the `InferenceRefunded` projector,
-- where the chain has already reported that the order left the book and the deadline
-- only answers WHY: `_finalizeTaker` is physically unreachable past the deadline, and
-- both expiry branches are unreachable before it.

comment on column inference_orders.deadline is
    'Unix seconds after which the book may expire this order; NULL when the chain '
    'value is 0. NULL means good-till-cancel only on a BUY: the contract rejects a '
    'zero-deadline SELL offer as malformed, so NULL on a SELL row is missing data, '
    'not a GTC ask. A subscription BUY is not a third case — its term is fixed at one '
    'month in the contract and the order carries no duration of its own, so its NULL '
    'is the GTC one and is permanent. Never compare it to the clock to derive a '
    'status — elapsed time alone closes nothing. The single exception is the '
    'InferenceRefunded projector: the chain has already removed the order there, and '
    'the deadline only says whether the cause was expiry (continuation expiry emits '
    'no InferenceOrderExpired at all).';
