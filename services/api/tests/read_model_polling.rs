// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Tests for the polling helper itself. They need no chain: what is checked is
// the classification of responses, and that control returns to the caller AS
// SOON AS the fact appears. This is what decision C rests on: if the helper
// looped until VALUES matched, a wrong remaining order quantity would look
// like a delay instead of a defect.

mod common;

use std::time::Duration;
use std::time::Instant;

use common::read_model::api;
use common::read_model::get_json;
use common::read_model::poll_read_with;
use common::read_model::read_phases_enabled_from;
use common::read_model::GetOutcome;
use common::read_model::Probe;
use common::read_model::PROBE_TIMEOUT_FLOOR;
use dodex_infrastructure::indexer_repo::CAPTURE_STREAM;
use sqlx::PgPool;

/// Short budget for the ordinary-deadline test — and it MUST be larger than
/// `POLL_INTERVAL` (2s). Otherwise `poll_read_with` would exit after the very
/// first probe and take the exhausted-budget branch: that branch's message
/// carries both the phase label and the last observation, so a swapped branch
/// would go unnoticed, while the ordinary-deadline branch — the one carrying
/// the triage hints — would be left untested.
const TINY: Duration = Duration::from_secs(3);

async fn purge(pool: &PgPool, ob: &str) {
    sqlx::query("delete from raw_events where src_address = $1")
        .bind(ob)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address = $1")
        .bind(ob)
        .execute(pool)
        .await
        .unwrap();
}

/// Capture cursor at the head. Without it the read path treats capture as
/// stale: `capture_stale` = `not coalesce((select … from indexer_cursors …),
/// false)`, and when the row is absent the subquery yields NULL ⇒ `not false`
/// ⇒ `true`. After `drop/create database` (Task 0 step 4) the row is absent
/// by definition.
async fn seed_at_head(pool: &PgPool) {
    sqlx::query(
        "insert into indexer_cursors (stream_name, cursor, at_head, updated_at) \
         values ($1, 'c', true, now()) \
         on conflict (stream_name) do update set at_head = true, updated_at = now()",
    )
    .bind(CAPTURE_STREAM)
    .execute(pool)
    .await
    .expect("seed at_head");
}

#[tokio::test]
async fn a_ready_probe_returns_at_once_so_a_wrong_value_fails_fast() {
    // The point is not "the loop works" but "control returns immediately":
    // field-level checks are the caller's job, so a wrong value lands in
    // `failures` the instant the row appears, not once the budget expires.
    let started = Instant::now();
    let v =
        poll_read_with("probe-ready", Duration::from_secs(30), || async { Probe::Ready(42_i32) })
            .await
            .expect("ready fact");
    assert_eq!(v, 42);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a ready fact must return immediately, otherwise a wrong value gets \
         masked as a delay"
    );
}

#[tokio::test]
async fn a_pending_probe_is_retried_until_it_is_ready() {
    let mut left = 2;
    let v = poll_read_with("probe-pending", Duration::from_secs(30), || {
        let ready = left == 0;
        left -= 1;
        async move {
            if ready {
                Probe::Ready("done")
            } else {
                Probe::Pending("not there yet".into())
            }
        }
    })
    .await
    .expect("the probe must eventually ripen");
    assert_eq!(v, "done");
}

#[tokio::test]
async fn the_budget_expires_with_the_last_observation_named() {
    // Return `Err`, not a panic: panicking mid-scenario is not an option —
    // orders are not yet settled. And the text MUST carry the LAST
    // observation: "the fact did not arrive" alone does not distinguish an
    // empty result set from a book stuck in discovering.
    let err = poll_read_with::<i32, _, _>("probe-timeout", TINY, || async {
        Probe::Pending("the book is still discovering".into())
    })
    .await
    .expect_err("the budget must expire");
    assert!(err.contains("the book is still discovering"), "no last observation: {err}");
    assert!(err.contains("probe-timeout"), "no phase label: {err}");
    // The ORDINARY-deadline branch, not the exhausted one. Without this
    // assert the two lines above pass on either branch, so swapping one for
    // the other (e.g. by shrinking `TINY` below `POLL_INTERVAL`) would go
    // unnoticed — and with it, the triage hints this branch exists to carry
    // would quietly disappear.
    assert!(
        err.contains("check the reconciler"),
        "expected the ordinary-deadline branch with its triage hint: {err}"
    );
    assert!(
        !err.contains("spent earlier"),
        "this is the deadline branch, not the exhausted one: {err}"
    );
}

#[tokio::test]
async fn an_exhausted_budget_says_so_instead_of_reporting_zero_seconds() {
    // The exhausted-budget branch is the only one the SHARED clock (decision
    // E) can lead to: chained waits tick the same budget, and a late phase
    // can be left with less than one poll interval. Without this test the
    // promise "gets a distinct wording" has nothing backing it.
    let err = poll_read_with::<i32, _, _>("probe-exhausted", Duration::ZERO, || async {
        Probe::Pending("the book is still discovering".into())
    })
    .await
    .expect_err("an exhausted budget must return Err");
    assert!(err.contains("spent earlier"), "wrong wording: {err}");
    assert!(!err.contains("within 0s"), "this is exactly the text the branch must displace: {err}");

    // The matching fact the `ReadBudget::left()` doc comment rests on: one
    // probe is made EVEN with a zero budget, because the probe runs BEFORE
    // the deadline check. Otherwise a burned-out budget would silently skip
    // a fact that had already arrived by then.
    let v =
        poll_read_with("probe-exhausted-ready", Duration::ZERO, || async { Probe::Ready(7_i32) })
            .await
            .expect("one probe is still made even with a zero budget");
    assert_eq!(v, 7);

    // And the MIDDLE of the window that a strict zero does not cover: a
    // remainder smaller than the poll interval. Here the exit must happen
    // right after the first probe — otherwise the loop goes to sleep for the
    // full interval, makes a second probe, and overruns the remainder, and
    // the "single probe" wording becomes a lie. The timing assert catches
    // exactly that: no sleep happened.
    let started = Instant::now();
    let err =
        poll_read_with::<i32, _, _>("probe-sub-interval", Duration::from_millis(500), || async {
            Probe::Pending("the book is still discovering".into())
        })
        .await
        .expect_err("a remainder smaller than the interval must return Err");
    assert!(err.contains("spent earlier"), "wrong wording: {err}");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the exit must happen after the FIRST probe, with no sleep for the full interval"
    );
}

#[tokio::test]
async fn a_stalled_probe_is_bounded_by_the_floor_instead_of_hanging() {
    // Every probe is bounded by max(remaining, PROBE_TIMEOUT_FLOOR): an
    // in-process request that stalls (a DB lock, an exhausted pool) must
    // surface as an expired budget with the stall NAMED — not hang the loop
    // past every budget into nextest's 600s kill, which would lose the
    // collected `failures` before scenario cleanup. Zero budget exercises the
    // tightest case: the single-probe contract holds (the probe still runs),
    // but bounded by the floor rather than awaited without limit.
    let started = Instant::now();
    let err = poll_read_with::<i32, _, _>("probe-stalled", Duration::ZERO, || async {
        std::future::pending::<Probe<i32>>().await
    })
    .await
    .expect_err("a stalled probe must come back as Err, not hang");
    assert!(err.contains("still running"), "the stall must be named: {err}");
    assert!(err.contains("spent earlier"), "zero budget keeps the exhausted wording: {err}");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= PROBE_TIMEOUT_FLOOR,
        "the single probe keeps its floored allowance: {elapsed:?}"
    );
    assert!(
        elapsed < PROBE_TIMEOUT_FLOOR + Duration::from_secs(2),
        "the stall must be cut off at the floor, not awaited: {elapsed:?}"
    );
}

#[tokio::test]
async fn a_fatal_probe_does_not_wait_out_the_budget() {
    // A terminal outcome must exit at once: a 400 is a typo in the URL, and
    // waiting would turn it into an expired budget.
    let started = Instant::now();
    let err = poll_read_with::<i32, _, _>("probe-fatal", Duration::from_secs(30), || async {
        Probe::Fatal("400 on /orders — bad URL".into())
    })
    .await
    .expect_err("a terminal outcome must return Err");
    assert!(err.contains("400"), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a terminal outcome does not wait out the budget"
    );
}

#[tokio::test]
async fn a_fail_closed_503_is_retryable_and_a_bad_request_is_not() {
    // The most valuable test in this file. `503` is a legitimate answer from
    // the gate, and a test that treats it as failure turns red on a healthy
    // system. `400` is the opposite.
    //
    // The URL here uses `tokenContract=` ON PURPOSE: the gate on unprojected
    // rows only fires when `token_contract.is_some() && side != BUY &&
    // statuses ∋ LIVE` — the request-shape guard in
    // `inference_read_repo::list_inference_orders_impl` (`asks_about_tc &&
    // scopes_live_sells`), which gates all three fail-closed arms rather than
    // being one of them. No scenario phase builds that shape; this is the only
    // test that does.
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let ob = "0:rmp_gate";
    purge(&pool, ob).await;
    seed_at_head(&pool).await;
    // A visible book with precision filled in: otherwise the refusal would
    // come from the scale guard, and the test would go green without
    // touching the gate under test.
    //
    // The seed is DELIBERATELY minimal — that is enough for `/orders`. Do not
    // reuse it for `/markets`: without `platform_fee_bps`, `quote_token_type`,
    // `tick_size` and the rest, the same row fails closed under IX-GATE-10.
    sqlx::query(
        "insert into inference_markets
           (orderbook_address, created_at_chain, last_reconciled_at,
            price_precision, quantity_precision)
         values ($1, now(), now(), 9, 0)",
    )
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "insert into raw_events (msg_id, chain_order, created_at, created_at_chain,
                                 src_address, dst_address, event_type, body_json, decoded)
         values ('rmp-gate-1','00rmp-1', now(), now(), $1, $1, null, '{}'::jsonb, null)",
    )
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();

    let url = api(&format!(
        "orders?inferenceOrderBookAddress={ob}&tokenContract=0:rmp_unknown&status=LIVE"
    ));

    // Collected, not asserted in place, and `purge` below runs unconditionally
    // after every check — the same discipline the on-chain scenario binaries
    // use (`common::read_model`). This test seeds a book AND an undecodable
    // `raw_events` row under `ob`; a `panic!` before cleanup would leave both
    // behind in whatever database `TEST_DATABASE_URL` names. On the stand
    // that is the live indexer database, and wave 2's observer counts books
    // and events inside the run window — a leaked row would turn it red for
    // a reason that is not the observer's.
    let mut failures: Vec<String> = Vec::new();

    match get_json(&service, &url).await {
        GetOutcome::Retry(why) => {
            if !why.contains("503") {
                failures.push(format!("refusal not recognized as 503: {why}"));
            }
        }
        other => failures.push(format!(
            "the gate must refuse with 503 while an unprojected row hangs under the book; got {}",
            match other {
                GetOutcome::Ok(_) => "200".to_string(),
                GetOutcome::Fatal(f) => f,
                GetOutcome::Retry(_) => unreachable!(),
            }
        )),
    }

    // Remove the cause of the refusal — the same request must now succeed.
    sqlx::query("update raw_events set processed_at = now() where msg_id = 'rmp-gate-1'")
        .execute(&pool)
        .await
        .unwrap();
    seed_at_head(&pool).await; // the cursor may have gone stale during the test
    match get_json(&service, &url).await {
        GetOutcome::Ok(_) => {}
        other => failures.push(format!(
            "after removing the cause of the refusal the request must succeed; got {}",
            match other {
                GetOutcome::Retry(w) | GetOutcome::Fatal(w) => w,
                GetOutcome::Ok(_) => unreachable!(),
            }
        )),
    }

    // 400: the book address is not passed at all ⇒ -1102, and that is terminal.
    let bad = get_json(&service, &api("depth")).await;
    if !matches!(bad, GetOutcome::Fatal(_)) {
        failures
            .push("a missing required parameter is a test defect, not \"not there yet\"".into());
    }

    purge(&pool, ob).await;
    assert!(failures.is_empty(), "read_model_polling gate failures: {failures:#?}");
}

#[tokio::test]
async fn a_book_not_yet_visible_answers_404_and_is_retried() {
    // `get_json`'s `404 → Retry` arm has no other test, and it is the arm
    // every on-chain read phase hits while a book is still discovering — those
    // binaries need a live shellnet, so this is the only local coverage that
    // would catch a future edit making "not visible yet" terminal instead of
    // retryable.
    //
    // Exercising the simpler of the two conditions that collapse into it
    // (`inference_read_repo.rs`, `list_inference_trades_impl`'s comment: "an
    // unknown book and a never-reconciled one collapse to one client-visible
    // miss"): an address with NO `inference_markets` row at all, not a
    // seeded-but-unreconciled one. Nothing is inserted, so there is nothing
    // to `purge`.
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    let url = api("markets?inferenceOrderBookAddress=0:rmp_no_such_book");
    match get_json(&service, &url).await {
        GetOutcome::Retry(why) => assert!(why.contains("404"), "wrong wording: {why}"),
        other => panic!(
            "an address with no row must read as \"not visible yet\" (404), got {}",
            match other {
                GetOutcome::Ok(_) => "200".to_string(),
                GetOutcome::Fatal(f) => f,
                GetOutcome::Retry(_) => unreachable!(),
            }
        ),
    }
}

#[tokio::test]
async fn a_router_404_without_the_error_envelope_is_terminal() {
    // Salvo answers 404 for a misspelled path too, without the production
    // ErrorBody envelope. Only a body carrying code -1121 means "book not
    // visible"; retrying a router miss would burn the shared budget on a typo
    // and report it as a read-model timeout. The test above is this one's
    // mirror: the same status with the -1121 envelope stays retryable.
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    match get_json(&service, &api("no-such-endpoint")).await {
        GetOutcome::Fatal(why) => {
            assert!(why.contains("404"), "the router miss must be named: {why}")
        }
        other => panic!(
            "a 404 without the -1121 envelope must be terminal, got {}",
            match other {
                GetOutcome::Ok(_) => "200".to_string(),
                GetOutcome::Retry(w) => format!("Retry: {w}"),
                GetOutcome::Fatal(_) => unreachable!(),
            }
        ),
    }
}

#[test]
fn read_phases_need_an_indexer_not_merely_a_database() {
    // The gate that decides whether the inference binaries run their read
    // phases. It exists because `TEST_DATABASE_URL` answers the wrong question:
    // the shellnet lane sets it for a Postgres nobody writes to, and the phases
    // there polled for facts only an indexer produces until the whole budget was
    // gone. Every binary this gate governs is `#[ignore]`, so this unit is the
    // only thing standing between a mistake here and a lane that burns its whole
    // phase to conclude nothing.
    assert!(!read_phases_enabled_from(None), "unset means off — the default must be the safe one");
    assert!(!read_phases_enabled_from(Some("")), "an empty value is not an opt-in");
    assert!(!read_phases_enabled_from(Some("0")), "an explicit 0 is off");
    assert!(!read_phases_enabled_from(Some("yes")), "only the documented spellings count");
    assert!(read_phases_enabled_from(Some("1")), "the woodpecker stand sets exactly this");
    assert!(read_phases_enabled_from(Some("true")));
    assert!(read_phases_enabled_from(Some("TRUE")));
}
