-- 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
--
-- InferenceOrderBook gained an explicit expiry path: a resting order whose
-- deadline passes is removed by the book, which emits InferenceOrderExpired.
-- That is a fourth terminal status alongside FILLED and CANCELLED.
--
-- The status is set ONLY by the event. A row whose `deadline` already sits in
-- the past keeps its OPEN status until the chain says otherwise, so the read
-- model never disagrees with the book about what is still resting.

alter table inference_orders drop constraint inference_orders_status_check;

alter table inference_orders
    add constraint inference_orders_status_check
    check (status in ('OPEN', 'FILLED', 'CANCELLED', 'EXPIRED'));

-- Supersedes the column comment written when only buy orders carried a deadline
-- and subscriptions were an order-book concern. Both are stale: sell offers now
-- carry a deadline of their own, and the subscription order type is gone from
-- the book (it moved into TokenContract as weekly billing).
comment on column inference_orders.deadline is
    'Unix seconds after which the book may expire this order; NULL when the chain '
    'value is 0, i.e. good-till-cancel. Never compare it to wall-clock to derive a '
    'status: an order stays OPEN until InferenceOrderExpired arrives.';
