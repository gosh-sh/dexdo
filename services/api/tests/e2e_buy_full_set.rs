// End-to-end smoke test for `POST /api/v1/buyFullSet` against a real
// shellnet OrderBook. Deploys a fresh PMP + OrderBook per run
// (which itself runs an initial `splitFullSet` to seed MM liquidity
// and bring the market into `TRADING`), then drives the production
// router for a second `splitFullSet` from the same trading PN — the
// trader-side production path of "deposit MM liquidity on market"
// from the branch name.
//
// The test verifies the public surface end to end:
//   - POST returns 200 with the two-field acceptance envelope;
//   - the chain debits `_balance[tokenType]` synchronously inside
//     `PrivateNote.splitFullSet` (see the function body in
//     `contracts/PrivateNote.sol`);
//   - the asynchronous `onSplitAccepted` callback clears the PN's
//     `_busy` flag (polled directly via `Dex::get_private_note_details`).
//
// What is NOT covered here: the AWAITING_FREEZE branch (where the
// first split also activates the OrderBook). `deploy_ephemeral_market`
// already runs the activating split during setup; covering
// AWAITING_FREEZE would need a deploy variant that stops before its
// internal splitFullSet — out of scope for this test.
//
// Marked `#[ignore]` because it needs:
//   - TEST_DATABASE_URL (test Postgres up — see README.md#test-postgres)
//   - reachable shellnet endpoint
//   - `tests/fixtures/test_pns.json` extended to slot 4 (the fifth PN)
//     via `mint_pn_pool` — `TestPnPool::slot(4)` panics with a clear
//     "top up via mint_pn_pool" message until the pool is extended.
//
// Run explicitly:
//
//   cargo test -p dodex-api --test e2e_buy_full_set -- --ignored --nocapture
//
// === SECURITY NOTE ===
// Same shellnet-only constraint as `e2e_order.rs`'s SECURITY NOTE —
// see `tests/fixtures/README.md` `[SHELLNET-TESTKEYS]`.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::canonical_query;
use common::deploy_market::deploy_ephemeral_market;
use common::deploy_market::DeployOptions;
use common::e2e_setup::db_pool;
use common::e2e_setup::provision_account;
use common::e2e_setup::upsert_market;
use common::e2e_setup::SHELLNET_ENDPOINT;
use common::now_ms;
use common::sign;
use common::test_pns::TestPnPool;
use dodex_api::testkit::build_router;
use dodex_api::testkit::AppState;
use dodex_api::testkit::SharedAuth;
use dodex_api::testkit::SharedChainSender;
use dodex_api::testkit::SharedRepo;
use dodex_chain::Dex as RawDex;
use dodex_infrastructure::auth::PostgresAuthenticator;
use dodex_infrastructure::chain_sender::DexChainSender;
use dodex_infrastructure::config::AuthSection;
use dodex_infrastructure::postgres_repo::PostgresReadModelRepository;
use dodex_infrastructure::tvm_hash::stake_hash;
use num_bigint::BigUint;
use salvo::http::StatusCode;
use salvo::test::ResponseExt;
use salvo::test::TestClient;
use salvo::Service;
use serde::Deserialize;
use serde_json::json;

/// Slot 4 per `tests/fixtures/README.md` — buyFullSet has its own PN
/// so a parallel `cargo nextest run` does not contend with the four
/// other e2e tests on `_busy`.
const BUY_FULL_SET_SLOT: usize = 4;

/// 10 NACKL of collateral at decimals=9. Small enough to leave the PN
/// solvent after the ~300 NACKL deploy already spent (see
/// `tests/fixtures/README.md#orderbook-fixture`); large enough to be
/// distinguishable from any incidental NACKL noise on the PN balance.
const COLLATERAL_HUMAN: &str = "10";
const COLLATERAL_RAW: u128 = 10_000_000_000;

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL + shellnet + tests/fixtures/test_pns.json slot 4"]
async fn buy_full_set_against_shellnet() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info,ackinacki_kit=debug,dodex_infrastructure::chain_sender=debug")
        .try_init();

    let Some((pool, kek)) = db_pool().await else {
        eprintln!("[e2e_buy_full_set] TEST_DATABASE_URL not set, skipping");
        return;
    };

    let pn_pool = TestPnPool::load();
    let trader = pn_pool.slot(BUY_FULL_SET_SLOT).clone();

    // `deploy_ephemeral_market` ends with its own splitFullSet, so by
    // the time this returns the market is in TRADING and the trader-PN
    // already holds some outcome tokens. The buyFullSet we send below
    // is a SECOND split that accumulates onto the existing stake — see
    // `PrivateNote.onSplitAccepted` in `contracts/PrivateNote.sol`.
    let market = deploy_ephemeral_market(
        vec![SHELLNET_ENDPOINT.to_string()],
        &trader,
        DeployOptions::default(),
    )
    .await
    .expect("deploy ephemeral market");

    let outcome_for_symbol = market.outcome_name.replace(' ', "-");
    let pmp_short = &market.pmp_address[..16.min(market.pmp_address.len())];
    let market_name = format!("PM-E2E-{pmp_short}");
    let symbol = format!("{market_name}-{outcome_for_symbol}");
    upsert_market(&pool, &market, &market_name, &symbol).await;

    let chain_sender: SharedChainSender = Arc::new(
        DexChainSender::new(
            vec![SHELLNET_ENDPOINT.to_string()],
            Duration::from_secs(30),
            Duration::from_secs(30),
            Duration::from_secs(30),
            Duration::from_secs(30),
            Duration::from_secs(30),
        )
        .expect("DexChainSender::new"),
    );

    let (api_key, secret_hex) = provision_account(&pool, &kek, &trader).await;

    let repo: SharedRepo = Arc::new(PostgresReadModelRepository::new(pool.clone()));
    let auth_config = AuthSection {
        kek_hex: "ab".repeat(32),
        default_recv_window_ms: 5_000,
        max_recv_window_ms: 60_000,
        seed_accounts: false,
    };
    let authenticator: SharedAuth =
        Arc::new(PostgresAuthenticator::new(pool.clone(), kek.clone(), &auth_config));
    let service = Service::new(build_router(AppState::new(
        repo,
        authenticator,
        chain_sender,
        Arc::new(common::FakePnStateReader::default()),
        Arc::new(common::FakeReferenceRepo::with_seeded()),
    )));

    // Raw chain handle for the before/after balance probe. Shares no
    // state with the API's `DexChainSender` — that one would block
    // on its own timeout config and is not designed for read getters.
    let raw_dex = RawDex::from_endpoints(vec![SHELLNET_ENDPOINT.to_string()])
        .expect("RawDex::from_endpoints");

    // Wait for `_busy` to clear before reading the pre-balance.
    // `deploy_ephemeral_market`'s splitFullSet sets the PN busy until
    // `onSplitAccepted` fires; reading mid-callback would give a stale
    // pre-balance that includes the in-flight candidateAmount.
    wait_pn_not_busy(&raw_dex, &trader.address, "pre-balance").await;

    let pre_balance = read_nackl_free_balance(&raw_dex, &trader.address, market.token_type).await;
    eprintln!("[e2e_buy_full_set] pre-balance _balance[{}] = {pre_balance}", market.token_type,);

    // Snapshot the per-outcome `_stakes[hash].amount` array before our
    // POST. The deploy's own splitFullSet already filled it with some
    // non-zero amounts; the post-snapshot must be strictly greater on
    // every outcome. `EphemeralMarket` carries `event_id` and
    // `oracle_list_hash` as `0x`-prefixed hex strings (the format
    // `OracleEventList.getEvents` and `Pmp.getDetails` emit for
    // `uint256` map keys), so the parse is base-16 after the prefix.
    let stake_key = stake_hash(
        &parse_uint256_hex(&market.event_id),
        &parse_uint256_hex(&market.oracle_list_hash),
        market.token_type,
    )
    .expect("compute stake_hash");
    let pre_stake = read_stake_amounts(&raw_dex, &trader.address, &stake_key)
        .await
        .expect("pre-snapshot _stakes[hash].amount must exist after deploy's split");
    eprintln!("[e2e_buy_full_set] pre-stake amounts = {pre_stake:?}");

    let body = serde_json::to_vec(&json!({
        "marketAddress": market.pmp_address,
        "collateral": COLLATERAL_HUMAN,
    }))
    .unwrap();

    let ts = now_ms();
    let canonical = canonical_query(&[("recvWindow", "5000"), ("timestamp", &ts.to_string())]);
    let sig = sign(&secret_hex, &canonical, &body);

    let mut resp = TestClient::post("http://test/api/v1/buyFullSet")
        .add_header("X-DODEX-APIKEY", api_key, true)
        .add_header("content-type", "application/json", true)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .body(body)
        .send(&service)
        .await;

    let status = resp.status_code;
    let body = resp.take_string().await.expect("response body");

    // No state to clean up on the chain side (splitFullSet is not
    // reversible — the deposited collateral is now stake), so a bare
    // `assert!` is fine here. The trader-PN holding extra outcome
    // tokens does not interfere with future test runs because every
    // run deploys a fresh PMP + OrderBook with its own `_stakes` map.
    assert_eq!(
        status,
        Some(StatusCode::OK),
        "POST /api/v1/buyFullSet status={status:?}; body: {body}",
    );

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct OkBody {
        market_address: String,
        transact_time: i64,
    }

    let parsed: OkBody =
        serde_json::from_str(&body).unwrap_or_else(|err| panic!("happy path body parse: {err}"));
    assert_eq!(parsed.market_address, market.pmp_address);
    assert!(parsed.transact_time > 0, "transactTime={}, want > 0", parsed.transact_time);

    // PrivateNote.splitFullSet decrements `_balance[tokenType]`
    // synchronously, before forwarding to PMP. So the synchronous
    // HTTP-200 already implies the debit landed. The `_busy` flag is
    // what we wait on — clearing it signals `onSplitAccepted` ran and
    // any leftover collateral was refunded back into `_balance`.
    // Reading after that gives the final post-refund balance.
    wait_pn_not_busy(&raw_dex, &trader.address, "post-balance").await;

    let post_balance = read_nackl_free_balance(&raw_dex, &trader.address, market.token_type).await;
    eprintln!("[e2e_buy_full_set] post-balance _balance[{}] = {post_balance}", market.token_type,);

    // Bounded assertion: SOME collateral was consumed, AT MOST the
    // requested `COLLATERAL_RAW`. The exact debit equals
    // `collateral - (collateral % Q)` where `Q = sum(u_k)` over the
    // gcd-reduced pool — Q depends on the deployer's stake shape.
    // For the standard symmetric deploy (`[100.2, 100.2]` NACKL → u_k
    // = [1, 1], Q = 2), the debit is exactly `COLLATERAL_RAW`. Asserting
    // the upper bound keeps the test correct under any future
    // tweak to `deploy_market.rs` stake amounts without re-deriving Q.
    let debited = pre_balance
        .checked_sub(post_balance)
        .unwrap_or_else(|| panic!("post-balance {post_balance} > pre-balance {pre_balance}"));
    assert!(
        debited > 0,
        "no NACKL debited — chain did not process splitFullSet (pre={pre_balance}, \
         post={post_balance})",
    );
    assert!(
        debited <= COLLATERAL_RAW,
        "debited {debited} > requested {COLLATERAL_RAW} — chain charged more than the \
         caller authorised (pre={pre_balance}, post={post_balance})",
    );

    // Outcome-token credit probe: balance debit alone proves the chain
    // ACCEPTED the splitFullSet, but the user-visible effect is the
    // outcome tokens credited via `onSplitAccepted` (line 688-style
    // path in `contracts/PrivateNote.sol`). Read `_stakes[hash].amount`
    // post-callback and assert each outcome's amount is strictly
    // greater than its pre-snapshot — closes the gap where a chain bug
    // could debit collateral without crediting outcome tokens.
    let post_stake = read_stake_amounts(&raw_dex, &trader.address, &stake_key)
        .await
        .expect("post-snapshot _stakes[hash].amount must exist");
    eprintln!("[e2e_buy_full_set] post-stake amounts = {post_stake:?}");

    assert_eq!(
        post_stake.len(),
        pre_stake.len(),
        "outcome count changed mid-test (pre={pre_stake:?}, post={post_stake:?})",
    );
    for (i, (pre_i, post_i)) in pre_stake.iter().zip(post_stake.iter()).enumerate() {
        assert!(
            post_i > pre_i,
            "outcome {i} amount did not increase: pre={pre_i}, post={post_i} \
             (full pre={pre_stake:?}, post={post_stake:?})",
        );
    }
}

/// Two-phase poll on `PrivateNote.getDetails().busyAddress`:
///
///   1. Up to 30 s waiting for `busy_address` to flip to `Some(...)` —
///      catches the window where `dex.split_full_set` has returned but
///      the gateway's read replica has not yet projected the on-chain
///      `_busy = pmpAddress` write. Without this phase, a fast caller
///      observes a stale `None` and exits, then reads `_balance` while
///      the chain is still mid-callback.
///   2. Up to 90 s waiting for `busy_address` to flip back to `None`,
///      signalling `onSplitAccepted` ran and any leftover collateral
///      was refunded into `_balance` (see `onSplitAccepted` in
///      `contracts/PrivateNote.sol`).
///
/// If phase 1 never observes `Some`, the operation might have landed
/// and completed inside one polling tick — sleep an extra 3 s before
/// returning so subsequent reads see settled state. Mirrors the
/// bee_dex integration suite's `wait_not_busy` pattern verbatim.
async fn wait_pn_not_busy(dex: &RawDex, pn_address: &str, phase: &str) {
    let mut saw_busy = false;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        match dex.get_private_note_details(pn_address).await {
            Ok(d) => {
                if d.busy_address.is_some() {
                    saw_busy = true;
                    break;
                }
            }
            Err(err) => {
                eprintln!(
                    "[e2e_buy_full_set] {phase} phase-1 getDetails failed (will retry): {err:?}",
                );
            }
        }
    }

    if !saw_busy {
        // Either the op landed + completed within polling resolution,
        // or chain has not picked up our external message yet. Give it
        // more time before assuming success.
        tokio::time::sleep(Duration::from_secs(3)).await;
        return;
    }

    for _ in 0..90 {
        match dex.get_private_note_details(pn_address).await {
            Ok(d) => {
                if d.busy_address.is_none() {
                    return;
                }
            }
            Err(err) => {
                eprintln!(
                    "[e2e_buy_full_set] {phase} phase-2 getDetails failed (will retry): {err:?}",
                );
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    panic!("[e2e_buy_full_set] {phase}: _busy did not clear within 90s after observed `Some`");
}

/// Read `PrivateNote._balance[tokenType]`. Returns 0 for an absent
/// token_type — the chain map omits zero entries, which is normal
/// when a PN has never received the asset. The deploy primes the
/// trader-PN with NACKL, so the pre-balance read finds the entry.
async fn read_nackl_free_balance(dex: &RawDex, pn_address: &str, token_type: u32) -> u128 {
    let details =
        dex.get_private_note_details(pn_address).await.expect("getDetails for balance probe");
    let key = token_type.to_string();
    details.balance.get(&key).copied().unwrap_or(0)
}

/// Parse an `EphemeralMarket`-style `0x`-prefixed lowercase hex string
/// into a `BigUint`. Panics on malformed input — the deploy_market
/// helper is responsible for emitting these, so a non-hex value means
/// the helper itself broke.
fn parse_uint256_hex(s: &str) -> BigUint {
    let stripped = s.strip_prefix("0x").unwrap_or_else(|| {
        panic!("expected 0x-prefixed hex uint256, got {s:?}");
    });
    BigUint::parse_bytes(stripped.as_bytes(), 16)
        .unwrap_or_else(|| panic!("parse hex uint256 {s:?}"))
}

/// Read `PrivateNote._stakes[stake_key].amount` as the per-outcome
/// `Vec<u128>`. Returns `None` if no entry exists for `stake_key` —
/// the deploy's own splitFullSet should have created one, so the
/// pre-snapshot returning `None` means setup did not actually land.
///
/// `stake_key` must be the same `0x`-prefixed lowercase 64-char hex
/// `dodex_infrastructure::tvm_hash::stake_hash` emits — that's the
/// shape the on-chain tvm_abi serializer uses for `uint256` map keys.
async fn read_stake_amounts(dex: &RawDex, pn_address: &str, stake_key: &str) -> Option<Vec<u128>> {
    let raw = dex.get_private_note_stakes(pn_address).await.expect("getStakes for amount probe");
    let entry = raw.stakes.get(stake_key)?;
    let arr = entry
        .get("amount")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("`_stakes[{stake_key}].amount` is not an array: {entry:?}"));
    let amounts: Vec<u128> = arr
        .iter()
        .map(|v| {
            v.as_str()
                .unwrap_or_else(|| panic!("amount element is not a string: {v:?}"))
                .parse::<u128>()
                .unwrap_or_else(|err| panic!("amount parse failed: {err:?}"))
        })
        .collect();
    Some(amounts)
}
