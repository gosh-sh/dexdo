// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use std::collections::HashMap;

use anyhow::anyhow;
use anyhow::Context;
use async_trait::async_trait;
use dodex_application::MarketReadRepository;
use dodex_domain::DepthSnapshot;
use dodex_domain::Market;
use dodex_domain::MarketAddress;
use dodex_domain::MarketName;
use dodex_domain::Outcome;
use dodex_domain::Symbol;
use sqlx::PgPool;

#[derive(Debug, Clone)]
pub struct PostgresReadModelRepository {
    pool: PgPool,
}

impl PostgresReadModelRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[derive(Debug, sqlx::FromRow)]
struct MarketJoinedRow {
    market_pk: i64,
    pmp_address: String,
    market_name: Option<String>,
    token_code: String,
    approved: bool,
    is_cancelled: bool,
    outcome_id: i32,
    outcome_name: String,
    symbol: String,
    price_precision: i32,
    quantity_precision: i32,
    tick_size: String,
    step_size: String,
    min_notional: String,
    max_batch_size: i32,
}

#[async_trait]
impl MarketReadRepository for PostgresReadModelRepository {
    /// Joins `markets` with `market_outcomes` and groups rows by market_id.
    /// `last_reconciled_at IS NOT NULL` filters out half-populated rows that
    /// only have the data from `PMPDeployed` but not from `getDetails`.
    async fn list_markets(
        &self,
        market_address: Option<&MarketAddress>,
    ) -> Result<Vec<Market>, anyhow::Error> {
        let address_filter = market_address.map(|m| m.0.as_str());

        let rows: Vec<MarketJoinedRow> = sqlx::query_as(
            r#"select
                   m.id            as market_pk,
                   m.pmp_address   as pmp_address,
                   m.market_id     as market_name,
                   m.token_code    as token_code,
                   m.approved      as approved,
                   m.is_cancelled  as is_cancelled,
                   mo.outcome_id          as outcome_id,
                   mo.outcome_name        as outcome_name,
                   mo.symbol              as symbol,
                   mo.price_precision     as price_precision,
                   mo.quantity_precision  as quantity_precision,
                   mo.tick_size           as tick_size,
                   mo.step_size           as step_size,
                   mo.min_notional        as min_notional,
                   mo.max_batch_size      as max_batch_size
                 from markets m
                 join market_outcomes mo on mo.market_id_fk = m.id
                where m.last_reconciled_at is not null
                  and ($1::text is null or m.pmp_address = $1)
                order by m.id, mo.outcome_id"#,
        )
        .bind(address_filter)
        .fetch_all(&self.pool)
        .await
        .context("select markets join market_outcomes")?;

        let mut markets: Vec<Market> = Vec::new();
        let mut index: HashMap<i64, usize> = HashMap::new();

        for row in rows {
            let pos = match index.get(&row.market_pk).copied() {
                Some(pos) => pos,
                None => {
                    let market_name = row.market_name.clone().ok_or_else(|| {
                        anyhow!(
                            "market {} has last_reconciled_at set but market_id (marketName) is NULL",
                            row.pmp_address
                        )
                    })?;
                    markets.push(Market {
                        market_address: MarketAddress(row.pmp_address.clone()),
                        market_name: MarketName(market_name),
                        status: derive_status(row.approved, row.is_cancelled),
                        quote_asset: row.token_code.clone(),
                        outcomes: Vec::new(),
                    });
                    let pos = markets.len() - 1;
                    index.insert(row.market_pk, pos);
                    pos
                }
            };

            markets[pos].outcomes.push(Outcome {
                outcome_id: row.outcome_id as u32,
                outcome_name: row.outcome_name,
                symbol: Symbol(row.symbol),
                price_precision: row.price_precision as u8,
                quantity_precision: row.quantity_precision as u8,
                tick_size: row.tick_size,
                step_size: row.step_size,
                min_notional: row.min_notional,
                max_batch_size: row.max_batch_size as u16,
            });
        }

        Ok(markets)
    }

    async fn get_depth(
        &self,
        _market_address: &MarketAddress,
        _symbol: &Symbol,
        _limit: u16,
    ) -> Result<DepthSnapshot, anyhow::Error> {
        // Depth materialization is Stage 8 (order_book_snapshots keyed by
        // (marketAddress, symbol) plus a refresh worker). Until then the
        // Postgres-backed repo cannot produce a snapshot.
        Err(anyhow!("depth read-model is not implemented yet"))
    }
}

fn derive_status(approved: bool, is_cancelled: bool) -> String {
    if is_cancelled {
        "CANCELLED".to_string()
    } else if !approved {
        "PENDING".to_string()
    } else {
        "TRADING".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_pending_when_not_approved() {
        assert_eq!(derive_status(false, false), "PENDING");
    }

    #[test]
    fn status_trading_when_approved_and_not_cancelled() {
        assert_eq!(derive_status(true, false), "TRADING");
    }

    #[test]
    fn status_cancelled_takes_precedence() {
        assert_eq!(derive_status(true, true), "CANCELLED");
        assert_eq!(derive_status(false, true), "CANCELLED");
    }
}
