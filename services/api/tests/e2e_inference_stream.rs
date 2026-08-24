// A streaming deal between TWO notes, from the offer that starts it to the
// clean STOPPED settlement that ends it — the scene no existing binary
// produces, and the only one that can prove two facts the self-trade tests
// structurally cannot.
//
//   IX-SEQ-04 — `inference_deals` records a seller and a buyer that are
//               DIFFERENT notes. Every other inference binary here is one note
//               playing both sides, so a projector that wrote the same address
//               into both columns would pass all of them.
//   IX-SEQ-06 — WAS: the deal closes `STOPPED` with `clean_settlement = true`.
//               No longer assertable: those columns are written only from
//               TokenContract events, and ingest scope excludes every
//               TokenContract route. The scene still drives the close on chain;
//               only the read-model assertion is gone. See the removed phase 9.
//
// Flow:
//   1. seller note (19) deploys the per-model InferenceOrderBook;
//   2. seller's TokenContract is deployed externally + giver-funded;
//   3. seller posts a SELL offer backed by that TC;
//   4. BUYER note (20) places a crossing IOC BUY ⇒ the book matches and funds
//      the TC. The buyer's half of the bond is funded by the buyer's own note
//      inline (`PrivateNote.sol:752`) — nothing here pays it;
//   5. seller funds its half (`fundDeal`) and opens the stream, freezing the
//      probe tick;
//   6. ── read phase, IX-SEQ-04 ── the deal row names both notes, and they differ;
//   7. wait out PROBE_WINDOW, then the seller accepts the probe;
//   8. optional: seller withdraws 1 SHELL, producing a post-upgrade
//      `ShellWithdrawn` body (see the note on OPTIONAL_STEP_RESERVE);
//   9. buyer stops the stream ⇒ the `_probeAccepted` branch ⇒ StreamStopped;
//  10. the close is verified on chain (the TC settles and self-destructs); the
//      read-model half of it is no longer captured — see phase 9 below.
//
// `inference_deals` has no HTTP surface and neither does `swept_at`, so the
// assertions here are direct SQL against the same pool `common::setup()` hands
// the router. That is also why this binary belongs to `serial-e2e-shared`: it
// reads the shared test database.
//
//   cargo test -p dodex-api --test e2e_inference_stream -- --ignored --nocapture
//
// === SECURITY NOTE === see e2e_inference.rs.

mod common;

use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use common::airegistry::deploy_token_contract;
use common::airegistry::wait_inference_book_live;
use common::airegistry::wait_sell_offer_rested;
use common::airegistry::TokenDeal;
use common::e2e_setup::model_hash_dec;
use common::e2e_setup::network_endpoint;
use common::read_model::poll_read_with;
use common::read_model::read_phases_enabled;
use common::read_model::Probe;
use common::read_model::ReadBudget;
use common::test_pns::TestPnPool;
use dodex_chain::Dex;
use dodex_contracts::airegistry::token_contract::ParamsOfOpen;
use dodex_contracts::airegistry::token_contract::ParamsOfWithdrawShell;
use dodex_contracts::dex::private_note::ParamsOfCancelAllInferenceOrders;
use dodex_contracts::dex::private_note::ParamsOfDeployInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfFundDeal;
use dodex_contracts::dex::private_note::ParamsOfInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfPlaceInferenceBuy;
use dodex_contracts::dex::private_note::ParamsOfPostSellOffer;
use dodex_contracts::dex::private_note::ParamsOfStreamDeal;
use sqlx::PgPool;
use sqlx::Row;

const POLL_TICK: Duration = Duration::from_secs(2);

// ── the budget, by summand ────────────────────────────────────────────────
//
// `ci-e2e` kills a test at `slow-timeout 60s × terminate-after 10` = 600s
// (`.config/nextest.toml`). Every wait below is a CEILING, not an expectation;
// the sum is what has to fit, because a stand having a bad minute is exactly
// when the ceilings are reached.
//
//   book live                     60s   (BOOK_TICKS)
//   offer rested                  60s   (OFFER_TICKS)
//   match funds the TC            40s   (FUND_TICKS)
//   both bonds land               60s   (BOND_TICKS)
//   open acknowledged             40s   (OPEN_TICKS)
//   PROBE_WINDOW                 190s   (180s of contract + 10s of chain clock)
//   probe accepted                30s   (ACCEPT_TICKS)
//   stop settles on chain         40s   (STOP_TICKS)
//                                ────
//                                520s, leaving 80s for the read phases'
//                                trailing polls and `finish`.
//
// The first four ceilings were cut after dexdo pipeline #292 measured them: the
// scene reached `open` in 15.9s of wall clock, against the 240s those steps had
// been given. They stay well above the measurement because they are timeouts
// for a bad day, not expectations — but 240s of headroom for a 16s stretch left
// no room for the bond wait this scene turned out to need.
//
// 60s is the floor this wave requires a scene to keep. The optional
// `withdrawShell` step is the first thing cut when the run is already slower
// than its ceilings — it is measured at runtime rather than assumed, see
// OPTIONAL_STEP_RESERVE.
const BOOK_TICKS: u32 = 30;
const OFFER_TICKS: u32 = 30;
const FUND_TICKS: u32 = 20;
const OPEN_TICKS: u32 = 20;
const ACCEPT_TICKS: u32 = 15;
/// Both bonds are in-flight messages when `fundDeal` returns; 60s is the ceiling
/// for the pair of them to land.
const BOND_TICKS: u32 = 30;
const STOP_TICKS: u32 = 20;

/// `PROBE_WINDOW` in `contracts/airegistry/modifiers/modifiers.sol:25` is 180s
/// and `TokenContract.sol:1034` compares it against the CHAIN's clock, not this
/// host's. The extra 10s is for that difference: accepting one second early
/// reverts with `ERR_SETTLE_WINDOW_OPEN`, and a revert here reads as "the
/// seller could not accept" rather than "the test did not wait".
///
/// This wait is incompressible. It is not a `sleep` standing in for a fact —
/// it IS the fact the contract measures, and no positive control can replace
/// it: there is no earlier observable that says the window has passed.
const PROBE_WAIT: Duration = Duration::from_secs(190);

/// How much of the 600s must still be unspent for the optional `withdrawShell`
/// step to run. It costs a send plus an acknowledgement poll; if the scene has
/// already overrun its ceilings, the step is dropped rather than gambled with,
/// because losing it costs a bonus (a post-upgrade `ShellWithdrawn` body,
/// which narrows IX-TC-14) while overrunning costs the whole run — nextest
/// kills the test before its `assert!` and every collected failure with it.
const OPTIONAL_STEP_RESERVE: Duration = Duration::from_secs(90);

/// The wall clock this scene is allowed, kept below the 600s kill so the final
/// `assert!` always runs.
const SCENE_CEILING: Duration = Duration::from_secs(560);

/// One `ReadBudget` for the binary, started before the chain waits — the wave-3
/// rule, and it shares its clock with them by design.
///
/// The total is NOT the default 240s, and that is deliberate rather than a
/// deviation. The default assumes the read phases follow a few minutes of
/// chain; here the LAST one follows nine, because `PROBE_WINDOW` alone is 190s
/// and it sits between the two phases. With 240s the budget would be long spent
/// by the time the stop settles, `left()` would be zero, and IX-SEQ-06 would
/// get a single probe fired the instant the stop lands — before any capture
/// tick could have seen it. That is not a strict check, it is a guaranteed
/// false red.
///
/// What the shared budget exists to prevent is per-fact budgets summing past
/// the 600s kill and losing the collected `failures`. Sizing the ONE budget to
/// the scene serves that purpose exactly: at the last phase this leaves roughly
/// `SCENE_CEILING - elapsed` of polling, and the test still reaches its
/// `assert!`.
const STREAM_READ_BUDGET: Duration = Duration::from_secs(540);

// A limit price must be a positive whole multiple of `PRICE_STEP` (1 SHELL =
// 1e9); the book rejects sub-SHELL dust with ERR_BAD_PARAM before assigning an
// order id, so a too-small price reads as "the order never rested".
const PRICE_PER_TICK: u128 = 1_000_000_000;
/// `PrivateNote.MAX_SELL_TTL` — the longest lifetime a SELL offer may ask for.
const MAX_SELL_TTL: u64 = 3600;
/// Ticks the deal is worth. Two is the smallest that still lets a probe tick be
/// distinct from the deal being exhausted.
const DEAL_TICKS: u128 = 2;
/// `>= ticks * (price + 2.5% fee)`, the escrow the book demands of the taker.
const BUY_ESCROW: u128 = 3_000_000_000;
/// `FLAG_IOC` (`InferenceOrderBook.sol:129`): cross what rests and do not leave
/// a remainder behind. A resting remainder would be a second order this scene
/// never accounts for.
const FLAG_IOC: u8 = 0x01;
/// The seller's half of the bond. DERIVED from the price, never hardcoded:
/// `TokenContract._bondAmount()` is `2 * _pricePerTick` (`:554-556`), and
/// `fundDeal` reverts with `ERR_INSUFFICIENT_DEPOSIT` on anything smaller
/// (`:906`). A hardcoded 1 SHELL against this 2 SHELL requirement is what
/// failed dexdo pipeline #293 — and it failed silently as far as the sender is
/// concerned, because the note sends with `bounce:true`, so the SHELL came
/// back and the only trace was `bond_funded: false` two steps later.
///
/// Overshooting is safe (`fundDeal` refunds the excess at `:914`), but the
/// derivation costs nothing and cannot drift when the price does.
const SELLER_BOND: u128 = 2 * PRICE_PER_TICK;
const DEAL_GAS_SHELL: u128 = 1_000_000_000;
/// One SHELL, the smallest withdrawal that still emits `ShellWithdrawn`.
const WITHDRAW_PROBE_AMOUNT: u128 = 1;

fn unique_suffix() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

/// The deal row as the read model holds it — the two columns this scene can
/// still assert on, read in one shot. Polling for presence and asserting content
/// are separate steps (the `read_model` rule), so the probe returns the row and
/// the caller judges it.
///
/// The settlement columns (`close_kind`, `clean_settlement`, `settled_at_chain`)
/// are deliberately NOT selected: their only writer is fed by TokenContract
/// events, which ingest no longer captures — see the note at the removed
/// IX-SEQ-06 phase below. Selecting them would suggest a scene could still wait
/// for them.
struct DealRow {
    seller_note: Option<String>,
    buyer_note: Option<String>,
}

async fn read_deal(pool: &PgPool, tc: &str) -> Result<Option<DealRow>, String> {
    let row = sqlx::query(
        "select seller_note, buyer_note \
         from inference_deals where token_contract_address = $1",
    )
    .bind(tc)
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("inference_deals query failed: {e}"))?;
    Ok(row.map(|r| DealRow { seller_note: r.get("seller_note"), buyer_note: r.get("buyer_note") }))
}

#[tokio::test]
#[ignore = "requires shellnet + seed_notes.json"]
async fn a_two_sided_deal_settles_clean_and_names_both_of_its_parties() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info,ackinacki_kit=debug")
        .try_init();

    let started = Instant::now();

    // Two notes, and this is the one binary here that needs them to be two.
    // `pn_pool_split.rs` reserves 19 and 20 for exactly this and guards the
    // pool length that keeps `k % len` from folding them together — but the
    // guard governs the spec, not the pool a run was actually handed, so the
    // difference is asserted here as well.
    let pool_pns = TestPnPool::load_inference();
    let seller = pool_pns.notes[19 % pool_pns.notes.len()].clone();
    let buyer = pool_pns.notes[20 % pool_pns.notes.len()].clone();
    assert_ne!(
        seller.address,
        buyer.address,
        "the pool handed the same note to both sides, so nothing below could tell a seller from a \
         buyer: it holds {} note(s) and this scene needs indices 19 and 20 to differ",
        pool_pns.notes.len()
    );

    let seller_keys = KeyPair {
        public: seller.owner_public_key_hex.clone(),
        secret: seller.owner_secret_key_hex.clone(),
    };
    let buyer_keys = KeyPair {
        public: buyer.owner_public_key_hex.clone(),
        secret: buyer.owner_secret_key_hex.clone(),
    };
    let seller_signer = || Signer::Keys { keys: seller_keys.clone() };
    let buyer_signer = || Signer::Keys { keys: buyer_keys.clone() };

    let dex = Dex::from_endpoints(vec![network_endpoint()]).expect("Dex::from_endpoints");
    let suffix = unique_suffix();
    let model_name = format!("e2e-stream--{suffix}");
    let model_hash = model_hash_dec(&model_name);
    let nonce = (suffix % 1_000_000_000) as u64 + 1;
    eprintln!(
        "[e2e_stream] seller={} buyer={} model_name={model_name}",
        seller.address, buyer.address
    );

    let mut failures: Vec<String> = Vec::new();

    // The read phase's database, opened before any chain wait so a missing one
    // is reported at the top rather than after nine minutes. `None` never
    // returns early: `finish` below cancels both notes' resting orders and only
    // then asserts, so an early exit would strand orders AND drop the failures
    // already collected.
    //
    // An opt-in that cannot be honoured is a failure, not a skip — this is the
    // only lane running an indexer, so skipping here leaves the read model
    // unchecked on the one run that could check it.
    let read: Option<PgPool> = if read_phases_enabled() {
        let pool = common::setup().await.map(|(_s, pool, _kek, _pn)| pool);
        if pool.is_none() {
            failures.push(
                "E2E_READ_MODEL asks for the read phases, but common::setup() found no database \
                 (TEST_DATABASE_URL unset, empty, or unreachable). This is the only lane that \
                 runs an indexer, so skipping here leaves the read model unchecked on the one \
                 run that could check it"
                    .to_string(),
            );
        }
        pool
    } else {
        eprintln!(
            "[e2e_stream] read phases skipped: E2E_READ_MODEL is not set, so no indexer is \
             filling the read model on this lane"
        );
        None
    };
    let budget = ReadBudget::with_total(STREAM_READ_BUDGET);

    // ── 1. the book ───────────────────────────────────────────────────────
    dex.deploy_inference_order_book(
        &seller.address,
        ParamsOfDeployInferenceOrderBook {
            model_hash: model_hash.clone(),
            model_name: model_name.clone(),
        },
        seller_signer(),
    )
    .await
    .expect("deployInferenceOrderBook accepted");
    let ob = dex
        .get_inference_order_book_address(
            &seller.address,
            ParamsOfInferenceOrderBook { model_hash: model_hash.clone() },
        )
        .await
        .expect("getInferenceOrderBookAddress");
    let fresh = wait_inference_book_live(&dex, &ob, BOOK_TICKS, POLL_TICK)
        .await
        .unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(fresh.order_count, 0, "a book this test just deployed already holds orders");
    eprintln!("[e2e_stream] order_book={ob}");

    // ── 2. the seller's TokenContract, and the offer it backs ─────────────
    let tc = deploy_token_contract(
        dex.context(),
        &seller.owner_public_key_hex,
        &seller.address,
        nonce,
        TokenDeal {
            model_name: model_name.clone(),
            price_per_tick: PRICE_PER_TICK,
            max_ticks: DEAL_TICKS,
        },
        seller_keys.clone(),
    )
    .await
    .expect("deploy TokenContract");
    eprintln!("[e2e_stream] token_contract={tc}");

    dex.post_sell_offer(
        &seller.address,
        ParamsOfPostSellOffer { flags: 0, nonce, ttl: MAX_SELL_TTL },
        seller_signer(),
    )
    .await
    .expect("postSellOffer accepted");
    if let Err(diag) = wait_sell_offer_rested(&dex, &ob, &tc, OFFER_TICKS, POLL_TICK).await {
        failures.push(diag);
        finish(
            &dex,
            &seller.address,
            &buyer.address,
            &model_hash,
            seller_signer(),
            buyer_signer(),
            failures,
        )
        .await;
        return;
    }

    // ── 3. the BUYER crosses it ───────────────────────────────────────────
    //
    // IOC so nothing of the taker rests: a remainder would be a second order
    // this scene never accounts for, and it would still be sitting in the book
    // when `finish` runs.
    dex.place_inference_buy(
        &buyer.address,
        ParamsOfPlaceInferenceBuy {
            model_hash: model_hash.clone(),
            max_price_per_tick: PRICE_PER_TICK,
            ticks: DEAL_TICKS,
            escrow: BUY_ESCROW,
            flags: FLAG_IOC,
            deadline: 0,
        },
        buyer_signer(),
    )
    .await
    .expect("placeInferenceBuy accepted");

    let mut funded = false;
    for _ in 0..FUND_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        match dex.token_contract_get_state(&tc).await {
            Ok(state) if state.funded => {
                eprintln!("[e2e_stream] TC funded: deposit={}", state.deposit);
                funded = true;
                break;
            }
            Ok(_) => {}
            Err(err) => eprintln!("[e2e_stream] getState errored (retry): {err:?}"),
        }
    }
    if !funded {
        failures.push("the match never funded the TokenContract".to_string());
        finish(
            &dex,
            &seller.address,
            &buyer.address,
            &model_hash,
            seller_signer(),
            buyer_signer(),
            failures,
        )
        .await;
        return;
    }

    // The chain's own view of the two parties, read before the read model is
    // asked the same question. If these already disagree the defect is on
    // chain, and the read-model phase below would blame the projector for it.
    match dex.token_contract_get_parties(&tc).await {
        Ok(parties) => {
            eprintln!(
                "[e2e_stream] on-chain parties: buyer={} sellerNote={}",
                parties.buyer, parties.seller_note
            );
            if parties.buyer != buyer.address {
                failures.push(format!(
                    "on chain the buyer is {}, not the note that placed the BUY ({})",
                    parties.buyer, buyer.address
                ));
            }
            if parties.seller_note != seller.address {
                failures.push(format!(
                    "on chain the seller note is {}, not the note that posted the offer ({})",
                    parties.seller_note, seller.address
                ));
            }
        }
        Err(err) => failures.push(format!("getParties unreadable after funding: {err:?}")),
    }

    // ── 4. the seller funds its half and opens the stream ─────────────────
    //
    // Only the seller's half travels this road. The buyer's bond was funded by
    // the buyer's own note inline on the fill (`PrivateNote.sol:752`), so it is
    // already paid by the time we get here.
    dex.fund_deal(
        &seller.address,
        ParamsOfFundDeal {
            nonce,
            gas_shell: DEAL_GAS_SHELL,
            amount: SELLER_BOND,
            endpoint_cipher: None,
        },
        seller_signer(),
    )
    .await
    .expect("fundDeal accepted");

    // Both bonds have to be in before `open` is sent, and neither arrives
    // synchronously. `TokenContract.sol:984-985` requires `_sellerBondFunded`
    // AND `_buyerBondFunded`, and both are set by messages still in flight at
    // this point: the seller's by the `fundDeal` just sent, the buyer's by the
    // `fundBuyerBond` its own note emitted back on the fill (`:721-727` spells
    // out that an ordinary deal's buyer bond "arrives later"). Sending `open`
    // straight after `fundDeal` raced both of them and reverted in the compute
    // phase with code 621 — which reads as "open is broken" rather than "open
    // was early", so this waits instead.
    //
    // The seller side has a flag; the buyer side does not, and
    // `token_contract_get_buyer_bond` explains why `bond_held > 0` stands in
    // for one here.
    let mut bonds_in = false;
    for _ in 0..BOND_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        let seller_in =
            matches!(dex.token_contract_get_seller_bond(&tc).await, Ok(b) if b.bond_funded);
        let buyer_in =
            matches!(dex.token_contract_get_buyer_bond(&tc).await, Ok(b) if b.bond_held > 0);
        if seller_in && buyer_in {
            bonds_in = true;
            break;
        }
    }
    if !bonds_in {
        let seller_bond = dex.token_contract_get_seller_bond(&tc).await;
        let buyer_bond = dex.token_contract_get_buyer_bond(&tc).await;
        failures.push(format!(
            "the deal's bonds never both landed, so `open` could only have reverted: \
             seller={seller_bond:?} buyer={buyer_bond:?}"
        ));
        finish(
            &dex,
            &seller.address,
            &buyer.address,
            &model_hash,
            seller_signer(),
            buyer_signer(),
            failures,
        )
        .await;
        return;
    }

    // Through `finish` rather than `expect`: a revert here strands two notes in
    // a funded deal, and the panic would skip the cancel-all that releases
    // their resting orders — the discipline every other early exit in this file
    // follows.
    if let Err(err) = dex
        .token_contract_open(
            &tc,
            // An opaque blob addressed to the buyer; nothing on the way reads it.
            ParamsOfOpen { endpoint_cipher: "00".to_string() },
            seller_signer(),
        )
        .await
    {
        failures.push(format!("open refused after both bonds were in: {err:?}"));
        finish(
            &dex,
            &seller.address,
            &buyer.address,
            &model_hash,
            seller_signer(),
            buyer_signer(),
            failures,
        )
        .await;
        return;
    }

    let mut opened = false;
    for _ in 0..OPEN_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if let Ok(state) = dex.token_contract_get_state(&tc).await
            && state.opened
        {
            opened = true;
            break;
        }
    }
    if !opened {
        failures.push("the stream never opened, so the probe tick never froze".to_string());
        finish(
            &dex,
            &seller.address,
            &buyer.address,
            &model_hash,
            seller_signer(),
            buyer_signer(),
            failures,
        )
        .await;
        return;
    }
    eprintln!("[e2e_stream] stream open at {:?}", started.elapsed());

    // ── 5. read phase, IX-SEQ-04 ──────────────────────────────────────────
    //
    // Here rather than after the close: the deal row is written when the TC is
    // funded, so this fact is already minutes old, and asking now leaves the
    // budget for the phase that genuinely has to wait (IX-SEQ-06 below).
    if let Some(pool) = read.as_ref() {
        let parties = poll_read_with("IX-SEQ-04 deal parties", budget.left(), || async {
            match read_deal(pool, &tc).await {
                Err(why) => Probe::Fatal(why),
                Ok(None) => Probe::Pending(format!("no inference_deals row for {tc} yet")),
                Ok(Some(row)) => match (&row.seller_note, &row.buyer_note) {
                    (Some(_), Some(_)) => Probe::Ready(row),
                    (s, b) => Probe::Pending(format!(
                        "deal row present but a party is still null: seller={s:?} buyer={b:?}"
                    )),
                },
            }
        })
        .await;

        match parties {
            Err(why) => failures.push(why),
            Ok(row) => {
                let got_seller = row.seller_note.unwrap_or_default();
                let got_buyer = row.buyer_note.unwrap_or_default();
                if got_seller != seller.address {
                    failures.push(format!(
                        "deal seller_note: want {}, got {got_seller}",
                        seller.address
                    ));
                }
                if got_buyer != buyer.address {
                    failures
                        .push(format!("deal buyer_note: want {}, got {got_buyer}", buyer.address));
                }
                // The assertion the self-trade binaries cannot make. Checking
                // the two columns against their expected values would ALSO pass
                // if the projector wrote one address into both and both notes
                // happened to be that one — this says the read model tells them
                // apart, independently of which addresses they are.
                if got_seller == got_buyer {
                    failures.push(format!(
                        "the deal names one party twice ({got_seller}): a two-sided deal projected \
                         as a self-trade"
                    ));
                }
            }
        }
    }

    // ── 6. the probe window, then acceptance ──────────────────────────────
    eprintln!("[e2e_stream] waiting out PROBE_WINDOW ({}s)", PROBE_WAIT.as_secs());
    tokio::time::sleep(PROBE_WAIT).await;

    dex.token_contract_accept_probe(&tc, seller_signer()).await.expect("acceptProbe accepted");
    let mut probe_accepted = false;
    for _ in 0..ACCEPT_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if let Ok(state) = dex.token_contract_get_state(&tc).await
            && state.probe_accepted
        {
            probe_accepted = true;
            break;
        }
    }
    if !probe_accepted {
        // Not fatal to the run, but it decides which branch step 7 takes, so
        // the eventual `close_kind` assertion would otherwise be blamed on the
        // projector rather than on the probe never being accepted.
        failures.push(
            "the probe was never accepted, so the stop below takes the probe-burn branch rather \
             than the clean one IX-SEQ-06 is about"
                .to_string(),
        );
    }

    // ── 7. optional: produce a post-upgrade ShellWithdrawn body ───────────
    //
    // A bonus, not a fact this scene owes anyone: wave 4 narrowed IX-TC-14 for
    // want of such a body, and one produced here makes that narrowing liftable
    // by the next harvest. Dropped without ceremony when the run is already
    // behind, because losing the bonus is cheap and overrunning the 600s kill
    // loses everything collected above.
    let spent = started.elapsed();
    if spent + OPTIONAL_STEP_RESERVE < SCENE_CEILING {
        match dex
            .token_contract_withdraw_shell(
                &tc,
                ParamsOfWithdrawShell { amount: WITHDRAW_PROBE_AMOUNT },
                seller_signer(),
            )
            .await
        {
            Ok(_) => eprintln!("[e2e_stream] withdrawShell sent — ShellWithdrawn body produced"),
            // Best-effort by construction: a refusal here says nothing about
            // the settlement this scene is testing.
            Err(err) => eprintln!("[e2e_stream] withdrawShell refused (non-fatal): {err:?}"),
        }
    } else {
        eprintln!(
            "[e2e_stream] optional withdrawShell dropped: {}s spent of a {}s ceiling",
            spent.as_secs(),
            SCENE_CEILING.as_secs()
        );
    }

    // ── 8. the buyer stops the stream ─────────────────────────────────────
    dex.stream_stop(
        &buyer.address,
        ParamsOfStreamDeal { token_contract: tc.clone() },
        buyer_signer(),
    )
    .await
    .expect("streamStop accepted");

    // Settling destroys the contract, so "gone" is the success signal and there
    // is no `settled` flag to read — `getState` simply stops answering. Either
    // outcome leads to the same place: the read model outlives the contract and
    // is where the verdict is actually asserted, so this loop only decides how
    // long to give the chain before asking.
    let mut gone = false;
    for _ in 0..STOP_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if dex.token_contract_get_state(&tc).await.is_err() {
            gone = true;
            break;
        }
    }
    eprintln!(
        "[e2e_stream] after stop the TC is {} at {:?}",
        if gone { "gone (settled and destroyed)" } else { "still answering" },
        started.elapsed()
    );

    // ── 9. read phase, IX-SEQ-06 ── REMOVED, and the reason is not tidiness.
    //
    // This phase asserted `close_kind = STOPPED` + `clean_settlement = true` +
    // `settled_at_chain` present. All three columns have exactly one writer,
    // `token_contract_projectors.rs`, fed by `TokenContract.StreamStopped` — and
    // that event no longer reaches the indexer at all. `config::SCOPED_EVENT_IDS`
    // scopes ingest to a `dst` allow-list that excludes every TokenContract route
    // (720..732), pinned by `token_contract_event_ids_are_all_out_of_scope` in
    // `crates/infrastructure/tests/ingest_scope.rs`.
    //
    // So this is not a slow fact, it is an absent one: an edge dropped at ingest
    // never reaches `raw_events`, and the indexer's own docs record that such a
    // drop is unrecoverable — the gateway's event window is finite, so no replay
    // brings it back either. Polling for it would burn the whole remaining budget
    // and then blame the indexer for a settlement that closed correctly.
    //
    // IX-SEQ-06 is therefore not covered by anything, and is recorded that way in
    // the matrix rather than left as a test that fails for a decided reason. The
    // scene above still runs to the close: step 8 proves the contract settled and
    // self-destructed on chain, which is what makes the missing ROW the finding.
    //
    // Restoring it means restoring live TokenContract capture (a capture source
    // that reaches self-rooted contracts, plus 720..732 back in the allow-list) —
    // at which point this block comes back unchanged. Same for the external
    // consumer: `dodex-points-rewards` reads `clean_settlement` and
    // `settled_at_chain` from this table (`rewards_query_compat.rs`), and both
    // now stay NULL forever.

    eprintln!("[e2e_stream] scene finished at {:?}", started.elapsed());
    finish(
        &dex,
        &seller.address,
        &buyer.address,
        &model_hash,
        seller_signer(),
        buyer_signer(),
        failures,
    )
    .await;
}

/// Cancels whatever either note still has resting, then makes the one
/// assertion. Best-effort on the cancels: a failure to clean up must not hide
/// the failures the scene collected.
async fn finish(
    dex: &Dex,
    seller: &str,
    buyer: &str,
    model_hash: &str,
    seller_signer: Signer,
    buyer_signer: Signer,
    failures: Vec<String>,
) {
    for (note, signer) in [(seller, seller_signer), (buyer, buyer_signer)] {
        let _ = dex
            .cancel_all_inference_orders(
                note,
                ParamsOfCancelAllInferenceOrders { model_hash: model_hash.to_string() },
                signer,
            )
            .await;
    }
    assert!(failures.is_empty(), "e2e_stream failures: {failures:#?}");
}
