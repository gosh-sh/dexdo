//! Oracle-side tests: fee withdrawal, oracle/market discovery.


use crate::common::context::create_dex;
use crate::common::context::TOKEN_TYPE_NACKL;

#[tokio::test]
#[ignore = "requires a live network with oracles already deployed on it"]
async fn test_discover_oracles_and_markets_via_dex() {
    let dex = create_dex();

    // Discover oracles — should find at least the ones our tests deployed
    let oracles = dex.discover_oracles().await.expect("discover_oracles");
    assert!(!oracles.is_empty(), "should find at least one oracle");
    eprintln!("Found {} oracles", oracles.len());
    for o in &oracles {
        eprintln!("  Oracle: {} @ {}", o.name, o.address);
    }

    // Discover all markets for NACKL
    let markets = dex.discover_markets(TOKEN_TYPE_NACKL).await.expect("discover_markets");
    eprintln!("Found {} markets", markets.len());
    for m in &markets {
        eprintln!(
            "  Market: {} @ {} (pool={}, approved={}, cancelled={}, resolved={:?})",
            m.event_name,
            m.pmp_address,
            m.total_pool,
            m.approved,
            m.is_cancelled,
            m.resolved_outcome
        );
    }

    // Active markets — subset
    let active =
        dex.discover_active_markets(TOKEN_TYPE_NACKL).await.expect("discover_active_markets");
    eprintln!("Found {} active markets", active.len());
    assert!(
        active.iter().all(|m| m.approved && !m.is_cancelled && m.resolved_outcome.is_none()),
        "all active markets must be approved, not cancelled, not resolved"
    );
}
