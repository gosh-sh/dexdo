// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Schema compatibility with the dodex-points-rewards query. The consumer lives in
// another repository and its tests run separately, so changing columns here breaks
// it silently — this test turns that breakage red on our side.

use std::env;
use std::time::Duration;

use dodex_infrastructure::database;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

/// Verbatim `resolve_deal` from
/// dodex-points-rewards/crates/infrastructure/src/indexer_reader.rs:52-57.
/// These five expressions are exactly what the consumer reads. It asks for
/// neither `finalized_ticks` nor `close_kind`, and requiring them here would be
/// inventing an obligation nobody took on.
const REWARDS_RESOLVE_DEAL: &str =
    "select orderbook_address, seller_note, buyer_note, clean_settlement, \
     (settled_at_chain is not null) as settled \
     from inference_deals where token_contract_address = $1";

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

#[tokio::test]
async fn the_rewards_resolve_deal_query_still_decodes() {
    let Some(pool) = setup().await else { return };
    let tc = "0:rewards_compat_probe";
    sqlx::query("delete from inference_deals where token_contract_address = $1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();
    // The row is mandatory: on an empty result set `query_as` decodes NOTHING, so a
    // column-type incompatibility would pass unnoticed — only the names would be
    // checked.
    sqlx::query(
        "insert into inference_deals \
         (token_contract_address, orderbook_address, seller_note, buyer_note, clean_settlement, settled_at_chain) \
         values ($1, '0:ob', '0:seller', '0:buyer', true, now())",
    )
    .bind(tc)
    .execute(&pool)
    .await
    .unwrap();

    let row: (Option<String>, Option<String>, Option<String>, Option<bool>, bool) =
        sqlx::query_as(REWARDS_RESOLVE_DEAL)
            .bind(tc)
            .fetch_one(&pool)
            .await
            .expect("the rewards query must stay valid against the dexdo schema");

    assert_eq!(row.0.as_deref(), Some("0:ob"));
    assert_eq!(row.1.as_deref(), Some("0:seller"));
    assert_eq!(row.2.as_deref(), Some("0:buyer"));
    assert_eq!(row.3, Some(true));
    assert!(row.4, "settled is derived from settled_at_chain");

    sqlx::query("delete from inference_deals where token_contract_address = $1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();
}
