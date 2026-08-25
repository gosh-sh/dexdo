// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
//! Run windows for the e2e observer. Every query here is bounded either by
//! INGEST time (`raw_events.created_at`) or by a set of addresses, so the tests
//! are safe in a shared database and do not depend on anyone else's rows — unlike
//! the global aggregates wave 1 lost time on.

use std::env;
use std::time::Duration;

use dodex_infrastructure::database;
use dodex_infrastructure::indexer_repo::IndexerRepository;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

async fn setup() -> Option<PgPool> {
    let _ = dotenvy::dotenv();
    let url = match env::var("TEST_DATABASE_URL") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return None;
        }
    };
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("TEST_DATABASE_URL connect");
    database::run_migrations(&pool).await.expect("run migrations");
    Some(pool)
}

/// A `raw_events` row with a given ingest age and a given decodability.
async fn seed_raw(pool: &PgPool, msg: &str, src: &str, event_type: Option<&str>, age_secs: i64) {
    sqlx::query(
        "insert into raw_events
           (msg_id, chain_order, created_at, created_at_chain, src_address, dst_address,
            event_type, body_json, decoded)
         values ($1, $1, now() - make_interval(secs => $4), now(), $2, null, $3, '{}'::jsonb,
                 case when $3::text is null then null else '{}'::jsonb end)
         on conflict (msg_id) do nothing",
    )
    .bind(msg)
    .bind(src)
    .bind(event_type)
    .bind(age_secs as f64)
    .execute(pool)
    .await
    .unwrap();
}

/// Marks a seeded row as drained by the projection loop. Separate from
/// `seed_raw` rather than a sixth parameter to it: seven call sites seed
/// unprocessed rows and none of them has an opinion about `processed_at`.
async fn mark_projected(pool: &PgPool, msg: &str) {
    sqlx::query("update raw_events set processed_at = now() where msg_id = $1")
        .bind(msg)
        .execute(pool)
        .await
        .unwrap();
}

async fn seed_book(
    pool: &PgPool,
    ob: &str,
    reconciled: bool,
    failed: bool,
    reason: Option<&str>,
    superseded: bool,
) {
    sqlx::query("delete from inference_markets where orderbook_address = $1")
        .bind(ob)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "insert into inference_markets
           (orderbook_address, created_at_chain, last_reconciled_at,
            last_reconcile_failed_at, last_reconcile_error, superseded_at)
         values ($1, now(),
                 case when $2 then now() else null end,
                 case when $3 then now() else null end,
                 $4,
                 case when $5 then now() else null end)",
    )
    .bind(ob)
    .bind(reconciled)
    .bind(failed)
    .bind(reason)
    .bind(superseded)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn the_pending_window_excludes_rows_ingested_before_the_run() {
    // The window exists precisely because the stand's database outlives pipelines:
    // without it, a row left behind by the run before last fails today's.
    let Some(pool) = setup().await else { return };
    let src = "0:obsq_pending";
    // The type is unique to this test. Otherwise a neighbouring test writing a row
    // of the same type would make the strict equality below flaky, while a loose
    // comparison could not tell "the window narrowed" from "the window was ignored".
    let ty = "InferenceOrderBook.ObsWindowProbe";
    let undec_new = "0:obsq_undec_new";
    let undec_old = "0:obsq_undec_old";
    let undec_scope: Vec<String> = [undec_new, undec_old].iter().map(|s| s.to_string()).collect();
    for a in [src, undec_new, undec_old] {
        sqlx::query("delete from raw_events where src_address = $1")
            .bind(a)
            .execute(&pool)
            .await
            .unwrap();
    }
    // Purge by PREFIX, not just by the four msg_ids below: a previous run that
    // panicked between the seeds and the tail cleanup left rows of exactly the
    // shape this file seeds — typed, decoded, unprocessed — and those are
    // eligible for `PENDING_PROJECTION_WHERE`. A prefix purge makes this file
    // self-healing across such a crash instead of accumulating.
    sqlx::query("delete from raw_events where msg_id like 'obsq-%'")
        .execute(&pool)
        .await
        .expect("prefix purge");
    seed_raw(&pool, "obsq-old", src, Some(ty), 7200).await;
    seed_raw(&pool, "obsq-new", src, Some(ty), 5).await;
    seed_raw(&pool, "obsq-undec-old", undec_old, None, 7200).await;
    seed_raw(&pool, "obsq-undec-new", undec_new, None, 5).await;

    let repo = IndexerRepository::new(pool.clone());
    let now = chrono::Utc::now().timestamp();
    let count_of = |rows: &[(String, i64)]| -> i64 {
        rows.iter().filter(|(t, _)| t == ty).map(|(_, n)| *n).sum()
    };

    let narrow = repo.pending_projection_since(now - 60).await.unwrap();
    assert_eq!(count_of(&narrow), 1, "the narrow window must contain exactly the fresh row");
    let wide = repo.pending_projection_since(now - 86_400).await.unwrap();
    assert_eq!(
        count_of(&wide),
        2,
        "the wide one holds both: so the window narrows rather than being ignored"
    );

    // Undecodable rows are counted SEPARATELY from projectable ones, by the same window.
    //
    // Exact sets come from the SCOPED variant, and the global counter is asserted
    // only one-sidedly (`>= 1`) further down. That split is deliberate:
    // `count_undecodable_since` returns a bare count, so unlike the address list
    // beside it its result cannot be narrowed to this test's rows after the fact
    // (`pending_projection_since` is unscoped too, but it returns per-type rows the
    // `count_of` closure above filters). Any claim about ITS exact value —
    // including a delta between two readings — is therefore at the mercy of an
    // outside writer. The competitor is known by name: `capture.rs`
    // (`persist_page_handles_mixed_decodable_and_undecodable_edges`) inserts an
    // undecodable row and then purges it, and a purge landing between two readings
    // breaks a delta even with a perfectly working window.
    let mut in_narrow = repo.undecodable_addresses_since(now - 60, &undec_scope).await.unwrap();
    in_narrow.sort();
    assert_eq!(
        in_narrow,
        vec![undec_new.to_string()],
        "only the fresh undecodable row falls into the narrow window"
    );
    let mut in_wide = repo.undecodable_addresses_since(now - 86_400, &undec_scope).await.unwrap();
    in_wide.sort();
    assert_eq!(
        in_wide,
        vec![undec_new.to_string(), undec_old.to_string()],
        "the wide one holds both"
    );
    // The global counter is the one the observer calls. The claim is one-sided and
    // does not depend on foreign rows: our fresh row is in the window and stays there.
    assert!(
        repo.count_undecodable_since(now - 60).await.unwrap() >= 1,
        "the fresh undecodable row must be counted by the global method too"
    );

    for a in [src, undec_new, undec_old] {
        sqlx::query("delete from raw_events where src_address = $1")
            .bind(a)
            .execute(&pool)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn the_book_scope_holds_only_books_with_events_in_this_run() {
    // This method decides WHO the observer judges at all: both the verdict check
    // and the wedged check take their scope from it. A wrong window here would
    // leave the neighbouring tests green while, on the stand, judging either every
    // book that ever existed or none of them. The shared `EVENTS_IN_WINDOW`
    // constant guarantees the two methods share the SAME predicate — but not that
    // the set is right, and "a book from the run before last does not fail today's"
    // would then rest on an unverified method.
    let Some(pool) = setup().await else { return };
    let fresh = "0:obsq_scope_fresh";
    let stale = "0:obsq_scope_stale";
    let silent = "0:obsq_scope_silent";
    let all: Vec<String> = [fresh, stale, silent].iter().map(|s| s.to_string()).collect();
    for ob in [fresh, stale, silent] {
        sqlx::query("delete from raw_events where src_address = $1")
            .bind(ob)
            .execute(&pool)
            .await
            .unwrap();
        seed_book(&pool, ob, true, false, None, false).await;
    }
    seed_raw(&pool, "obsq-scope-stale", stale, Some("InferenceOrderBook.InferenceFilled"), 7200)
        .await;

    let repo = IndexerRepository::new(pool.clone());
    let now = chrono::Utc::now().timestamp();

    let narrow = repo.inference_books_with_events_since(now - 60).await.unwrap();
    assert!(
        !narrow.contains(&fresh.to_string()),
        "a book with no events in the window is not in the scope"
    );
    assert!(
        !narrow.contains(&stale.to_string()),
        "a book whose only event is OLDER than the window is not in the scope — \
         otherwise one left by an aborted run would fail the next for a foreign reason"
    );

    seed_raw(&pool, "obsq-scope-fresh", fresh, Some("InferenceOrderBook.InferenceFilled"), 5).await;
    // The row is PROCESSED: at the tail of a run most of them are, and the scope
    // must see them. That is also why index 0007 cannot be partial on
    // `processed_at is null` the way both existing `src_address` indexes are.
    sqlx::query("update raw_events set processed_at = now() where msg_id = 'obsq-scope-fresh'")
        .execute(&pool)
        .await
        .unwrap();

    let narrow = repo.inference_books_with_events_since(now - 60).await.unwrap();
    assert!(
        narrow.contains(&fresh.to_string()),
        "a book with an event in the window must land in the scope, processed rows included"
    );
    assert!(
        !narrow.contains(&silent.to_string()),
        "a book with no events at all is not in the scope"
    );

    let wide = repo.inference_books_with_events_since(now - 86_400).await.unwrap();
    assert!(
        wide.contains(&stale.to_string()),
        "the stale book appears in the wide window — so the selection is made by the window itself"
    );

    for ob in [fresh, stale, silent] {
        sqlx::query("delete from raw_events where src_address = $1")
            .bind(ob)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("delete from inference_markets where orderbook_address = any($1)")
        .bind(&all)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_verdict_needs_a_reason_and_a_superseded_book_needs_none() {
    let Some(pool) = setup().await else { return };
    let visible = "0:obsq_visible";
    let failing_with = "0:obsq_failing_with";
    let failing_without = "0:obsq_failing_without";
    let superseded = "0:obsq_superseded";
    let discovering = "0:obsq_discovering";
    let broke_after_visible = "0:obsq_broke_after_visible";
    let scope: Vec<String> =
        [visible, failing_with, failing_without, superseded, discovering, broke_after_visible]
            .iter()
            .map(|s| s.to_string())
            .collect();

    seed_book(&pool, visible, true, false, None, false).await;
    seed_book(&pool, failing_with, false, true, Some("getVersion reverted"), false).await;
    seed_book(&pool, failing_without, false, true, None, false).await;
    seed_book(&pool, superseded, false, false, None, true).await;
    seed_book(&pool, discovering, false, false, None, false).await;
    // Visible AND carrying a failure mark: a book that worked and then broke. The
    // shape says that and not merely "failed once, ever", because three writers
    // agree on the mark — the visibility stamp clears it, a clean refresh cycle
    // clears it (`InferenceReconciler::clear_failure`), and `stamp_failure` sets
    // it. So a book that recovered has already left this list by its next clean
    // cycle. It is the class no gauge can show — the `failing` bucket counts this
    // book as `visible` — so hiding it here would leave a broken-right-now book
    // with no line of output anywhere.
    seed_book(&pool, broke_after_visible, true, true, Some("getOrder reverted"), false).await;

    let repo = IndexerRepository::new(pool.clone());
    let mut without = repo.inference_books_without_verdict(&scope).await.unwrap();
    without.sort();
    assert_eq!(
        without,
        vec![discovering.to_string(), failing_without.to_string()],
        "a verdict is missing for exactly the not-yet-resolved book and the one whose \
         failure carries NO TEXT; superseded is a full third verdict, not a softening"
    );

    let failing = repo.inference_failing_books(&scope).await.unwrap();
    assert_eq!(
        failing,
        vec![
            (broke_after_visible.to_string(), "getOrder reverted".to_string()),
            (failing_with.to_string(), "getVersion reverted".to_string()),
        ],
        "the printed failing list carries the reason and is deliberately WIDER than \
         the `failing` gauge bucket: it must hold the never-visible book AND the one \
         that broke after becoming visible. Adding `last_reconciled_at is null` here \
         to match the bucket would drop the second — the class no gauge can show, \
         since the bucket counts it as `visible`"
    );

    sqlx::query("delete from inference_markets where orderbook_address = any($1)")
        .bind(&scope)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn the_anchor_finds_a_visible_book_with_orders_and_events_in_the_window() {
    let Some(pool) = setup().await else { return };
    let ob = "0:obsq_anchor";
    seed_book(&pool, ob, true, false, None, false).await;
    sqlx::query("delete from inference_orders where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from raw_events where src_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    let repo = IndexerRepository::new(pool.clone());
    let window = chrono::Utc::now().timestamp() - 60;

    // Neither orders nor events — the anchor is empty.
    assert!(repo.inference_anchored_books_since(window).await.unwrap().iter().all(|a| a != ob));

    sqlx::query(
        "insert into inference_orders
           (orderbook_address, order_id, is_buy, price, amount_initial, amount_remaining,
            status, last_chain_order, chain_created_at, chain_updated_at)
         values ($1, 1, true, 1, 10, 10, 'OPEN', '00obsq', now(), now())",
    )
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();
    // An order exists but no event from this run: the anchor is still empty.
    assert!(repo.inference_anchored_books_since(window).await.unwrap().iter().all(|a| a != ob));

    seed_raw(&pool, "obsq-anchor-ev", ob, Some("InferenceOrderBook.InferenceOrderPlaced"), 5).await;
    assert!(repo.inference_anchored_books_since(window).await.unwrap().iter().any(|a| a == ob));

    // And outside the window it is empty again: the window really narrows rather than decorates.
    let future = chrono::Utc::now().timestamp() + 3600;
    assert!(repo.inference_anchored_books_since(future).await.unwrap().iter().all(|a| a != ob));

    sqlx::query("delete from inference_orders where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from raw_events where src_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn the_dex_anchor_counts_this_runs_order_book_rows_and_is_not_fed_by_inference() {
    let Some(pool) = setup().await else { return };
    let src = "0:obsq_dex";
    // A type of this test's own, for the reason the pending-window test gives
    // itself one: the query returns every `OrderBook.%` type in the window, and a
    // neighbour writing a real `OrderBook.OrderPlaced` would make the equalities
    // below flaky. All assertions read this type only.
    let ty = "OrderBook.ObsDexProbe";
    sqlx::query("delete from raw_events where msg_id like 'obsq-dex-%'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from raw_events where src_address = $1")
        .bind(src)
        .execute(&pool)
        .await
        .unwrap();

    let repo = IndexerRepository::new(pool.clone());
    let now = chrono::Utc::now().timestamp();
    let of = |rows: &[(String, i64, i64)]| -> (i64, i64) {
        rows.iter().filter(|(t, _, _)| t == ty).fold((0, 0), |(c, p), (_, rc, rp)| (c + rc, p + rp))
    };

    // Ingested before the run: outside the window, and the stand's database
    // outlives pipelines, so this is the row that would otherwise answer for a
    // run that captured nothing.
    seed_raw(&pool, "obsq-dex-old", src, Some(ty), 7200).await;
    mark_projected(&pool, "obsq-dex-old").await;
    assert_eq!(
        of(&repo.dex_capture_progress_since(now - 60).await.unwrap()),
        (0, 0),
        "a projected row from an earlier run must not satisfy this run's anchor"
    );

    // Captured in the window, not yet drained: visible, and honestly not projected.
    seed_raw(&pool, "obsq-dex-new", src, Some(ty), 5).await;
    assert_eq!(
        of(&repo.dex_capture_progress_since(now - 60).await.unwrap()),
        (1, 0),
        "an ingested row counts as captured and, until the loop drains it, not as projected"
    );

    mark_projected(&pool, "obsq-dex-new").await;
    assert_eq!(
        of(&repo.dex_capture_progress_since(now - 60).await.unwrap()),
        (1, 1),
        "draining the row moves it into the projected count without duplicating it"
    );

    // The wide window holds both, so the narrowing above was the window doing its
    // job rather than the query ignoring rows.
    assert_eq!(of(&repo.dex_capture_progress_since(now - 86_400).await.unwrap()), (2, 2));

    // The prefix anchors at the START. This is the assertion that keeps the anchor
    // meaningful: widened to a contains-match, inference traffic would answer for
    // the DEX half and the whole point of having two anchors would be gone.
    let inference_ty = "InferenceOrderBook.InferenceOrderPlaced";
    seed_raw(&pool, "obsq-dex-inference", src, Some(inference_ty), 5).await;
    mark_projected(&pool, "obsq-dex-inference").await;
    let rows = repo.dex_capture_progress_since(now - 60).await.unwrap();
    assert!(
        rows.iter().all(|(t, _, _)| t != inference_ty),
        "`InferenceOrderBook.` must not match the `OrderBook.` prefix: {rows:?}"
    );
    assert_eq!(of(&rows), (1, 1), "and the DEX counts are unchanged by it");

    sqlx::query("delete from raw_events where src_address = $1")
        .bind(src)
        .execute(&pool)
        .await
        .unwrap();
}
