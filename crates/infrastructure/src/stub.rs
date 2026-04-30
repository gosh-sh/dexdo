// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use async_trait::async_trait;
use dodex_application::MarketReadRepository;
use dodex_domain::DepthSnapshot;
use dodex_domain::Market;
use dodex_domain::MarketAddress;
use dodex_domain::MarketName;
use dodex_domain::Outcome;
use dodex_domain::PriceLevel;
use dodex_domain::Symbol;

const STUB_MARKET_ADDRESS: &str = "0:market-address";
const STUB_MARKET_NAME: &str = "PM-2026-ELECTION";

#[derive(Debug, Clone, Default)]
pub struct StubReadModelRepository;

#[async_trait]
impl MarketReadRepository for StubReadModelRepository {
    async fn list_markets(
        &self,
        market_address: Option<&MarketAddress>,
    ) -> Result<Vec<Market>, anyhow::Error> {
        let market = Market {
            market_address: MarketAddress(STUB_MARKET_ADDRESS.to_string()),
            market_name: MarketName(STUB_MARKET_NAME.to_string()),
            status: "TRADING".to_string(),
            quote_asset: "USDC".to_string(),
            outcomes: vec![
                Outcome {
                    outcome_id: 0,
                    outcome_name: "NO".to_string(),
                    symbol: Symbol(format!("{STUB_MARKET_NAME}-NO")),
                    price_precision: 3,
                    quantity_precision: 2,
                    tick_size: "0.001".to_string(),
                    step_size: "0.01".to_string(),
                    min_notional: "1".to_string(),
                    max_batch_size: 5,
                },
                Outcome {
                    outcome_id: 1,
                    outcome_name: "YES".to_string(),
                    symbol: Symbol(format!("{STUB_MARKET_NAME}-YES")),
                    price_precision: 3,
                    quantity_precision: 2,
                    tick_size: "0.001".to_string(),
                    step_size: "0.01".to_string(),
                    min_notional: "1".to_string(),
                    max_batch_size: 5,
                },
            ],
        };

        let items = match market_address {
            Some(addr) if addr.0 != market.market_address.0 => Vec::new(),
            _ => vec![market],
        };

        Ok(items)
    }

    async fn get_depth(
        &self,
        market_address: &MarketAddress,
        symbol: &Symbol,
        limit: u16,
    ) -> Result<DepthSnapshot, anyhow::Error> {
        let max_levels = usize::from(limit.max(1));

        let bids = vec![
            PriceLevel { price: "0.614".to_string(), quantity: "100.00".to_string() },
            PriceLevel { price: "0.613".to_string(), quantity: "25.50".to_string() },
        ]
        .into_iter()
        .take(max_levels)
        .collect();

        let asks = vec![
            PriceLevel { price: "0.616".to_string(), quantity: "50.00".to_string() },
            PriceLevel { price: "0.617".to_string(), quantity: "75.25".to_string() },
        ]
        .into_iter()
        .take(max_levels)
        .collect();

        Ok(DepthSnapshot {
            market_address: market_address.clone(),
            symbol: symbol.clone(),
            last_update_id: 1_027_024,
            bids,
            asks,
        })
    }
}
