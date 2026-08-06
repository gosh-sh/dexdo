// A note can put its own deal contract on chain, with no wallet behind it.
//
// Every other test in this suite deploys a `TokenContract` off the shared
// shellnet giver, because that is the cheap way to get one. The contracts are
// not designed around it. A seller in production has a note and nothing else —
// no operational wallet, no giver — and `PrivateNote.fundDeployShell` is the
// whole of how that works: the note ships SHELL to the address its own deal
// contract is going to occupy, and the seller-signed deploy then lands there.
//
// ## Why flag 16, and why that is the thing worth testing
//
// A deal contract is cross-dApp: it becomes the root of its own dApp the moment
// it exists. Native value does not cross that boundary, and ordinary ECC (flag
// 1) credits an address's SHELL ledger without making it deployable. Only a
// flag-16 send converts the credit into the new account's native gas, which is
// what an arriving deploy message needs to activate.
//
// That difference is also where the balances below are read. A flag-16 credit
// leaves the target's ECC SHELL at zero and its NATIVE balance up, so reading
// the SHELL ledger reports a funding that worked perfectly as nothing having
// happened at all. The amounts are not asserted exactly: what a credit becomes
// on the way into a native balance is not something measured here, and an
// assertion resting on a guess about it would be a statement about the guess.
// What IS exact is the debit — the note is the source, and that side is a plain
// ECC send off its own ledger.
//
// So the reading here is not "a balance went up". It is that a constructor sent
// afterwards — with no giver anywhere in the run — brings the contract alive.
// If `fundDeployShell` credited the address the wrong way, money would still
// leave the note and the deploy would still be accepted; the account would
// simply stay uninitialised. Nothing short of the deploy separates those two.
//
// ## What it is NOT allowed to do
//
// The call takes no address. The target is derived inside the note from its own
// key (`_ephemeralPubkey`) plus the nonce — so SHELL sent this way can only ever
// reach that note's own canonical `TokenContract`, and never an address a caller
// picked. The test pins that by deriving the address independently, from the
// deploy message itself, and checking the money arrived exactly there.
//
// ## The RootModel is watched, and must never move
//
// There used to be a second leg. `fundDeployShell(nonce, rootModelShell, tcShell)`
// also pre-funded the note's canonical RootModel, because a RootModel was then
// deployed by its owner as an external message and something had to put native
// gas at that address first. The super root deploys it now with an internal
// `new`, which carries its own value, so the leg and its parameter are gone.
//
// The RootModel address is still derived here and still checked — the claim has
// simply inverted. It used to be "a `rootModelShell` of 0 skips the leg"; it is
// now "there is no leg, so this call cannot reach that address under any
// argument". Watching an address a call is supposed to have no route to is the
// cheap way to notice the route coming back.
//
// `amount 0 = skip` is still contract-documented behaviour on the leg that
// remains, and it still gets its own negative below rather than being assumed.
//
//   cargo test -p dodex-api --test e2e_inference_funding -- --ignored --nocapture
//
// === SECURITY NOTE === see e2e_inference.rs.

mod common;

use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use common::airegistry::canonical_root_model_address;
use common::airegistry::plan_token_contract;
use common::airegistry::TokenDeal;
use common::e2e_setup::network_endpoint;
use common::test_pns::TestPn;
use common::test_pns::TestPnPool;
use dodex_chain::Dex;
use dodex_contracts::dex::private_note::ParamsOfFundDeployShell;

const POLL_TICK: Duration = Duration::from_secs(2);
const POLL_TICKS: u32 = 45; // 90s budget per wait.

/// What the deal address is funded with. It is the amount the giver-funded
/// route sends: a deploy target has to arrive at a balance that can actually
/// carry a constructor, and this is the only figure known to.
///
/// A smaller, "nominal" amount is not a cheaper way to say the same thing. An
/// earlier cut of this test sent a second target 3 SHELL to keep the two
/// distinguishable by amount; that account came into existence and its balance
/// came out at zero, which says a credit that small does not survive the trip.
/// An amount is a bad way to tell sends apart — a figure that arrives as zero
/// is indistinguishable from one that never left.
const TARGET_SHELL: u128 = 200_000_000_000;

/// Deal terms. Nothing trades here; they exist because the constructor demands
/// a coherent set and because the address depends on none of them — it comes
/// from the seller key and the nonce alone.
const PRICE_PER_TICK: u128 = 1_000_000_000;
const DEAL_TICKS: u128 = 2;

fn unique_suffix() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

fn signer_of(note: &TestPn) -> Signer {
    Signer::Keys {
        keys: KeyPair {
            public: note.owner_public_key_hex.clone(),
            secret: note.owner_secret_key_hex.clone(),
        },
    }
}

#[tokio::test]
#[ignore = "requires shellnet + seed_notes.json"]
async fn a_note_funds_and_deploys_its_own_deal_contract_without_a_wallet() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info,ackinacki_kit=debug")
        .try_init();

    let pool = TestPnPool::load();
    let seller = pool.notes[18 % pool.notes.len()].clone();
    let dex = Dex::from_endpoints(vec![network_endpoint()]).expect("Dex::from_endpoints");
    let suffix = unique_suffix();
    let nonce = (suffix % 1_000_000_000) as u64 + 1;
    let model_name = format!("e2e-funding--{suffix}");
    eprintln!("[e2e_funding] note={} nonce={nonce}", seller.address);

    let mut failures: Vec<String> = Vec::new();

    // ── 1. where the note's money is supposed to go, and where it is not ──
    //
    // Both addresses are worked out here, from outside the note: the deal
    // contract from the deploy message itself, the RootModel from the canonical
    // SuperRoot. The note derives the deal address internally and is never told
    // it, so an agreement between the two derivations is the only thing that
    // says the SHELL went where it was meant to. The RootModel is derived for
    // the opposite reason — it is the address this call must never reach.
    let pubkey = format!("0x{}", seller.owner_public_key_hex.trim_start_matches("0x"));
    let root_model = canonical_root_model_address(dex.context(), &pubkey)
        .await
        .expect("canonical RootModel address");
    let plan = plan_token_contract(
        dex.context(),
        &seller.owner_public_key_hex,
        &seller.address,
        nonce,
        TokenDeal {
            model_name: model_name.clone(),
            price_per_tick: PRICE_PER_TICK,
            max_ticks: DEAL_TICKS,
        },
        KeyPair {
            public: seller.owner_public_key_hex.clone(),
            secret: seller.owner_secret_key_hex.clone(),
        },
    )
    .await
    .expect("plan the TokenContract deploy");
    let tc = plan.address.clone();
    eprintln!("[e2e_funding] root_model={root_model} token_contract={tc}");

    let root_model_before = dex.self_rooted_account_shell(&root_model).await.expect("rm before");
    let tc_before = dex.self_rooted_account_shell(&tc).await.expect("deal address before");
    let note_before = dex.dex_account_shell(&seller.address).await.expect("note before").shell;
    eprintln!(
        "[e2e_funding] before: note={note_before} root_model=({} native, {} ecc, {}) \
         deal=({} native, {} ecc, {})",
        root_model_before.native,
        root_model_before.shell,
        root_model_before.acc_type,
        tc_before.native,
        tc_before.shell,
        tc_before.acc_type
    );
    // The deal address is derived from a nonce nobody has used, so there must be
    // nothing there. If there were, every delta below would be measuring
    // somebody else's account.
    if tc_before.acc_type == "Active" {
        failures.push(format!(
            "the deal address {tc} already runs code before anything was deployed; the nonce is \
             not fresh and this run would be reading another deal"
        ));
        assert!(failures.is_empty(), "e2e_funding failures: {failures:#?}");
    }

    // ── 2. the deal address, and only the deal address ────────────────────
    //
    // The call has one leg and this exercises it. What makes the run say
    // something beyond "a balance went up" is the address it is NOT allowed to
    // touch, read either side of the same call.
    dex.fund_deploy_shell(
        &seller.address,
        ParamsOfFundDeployShell { nonce, tc_shell: TARGET_SHELL },
        signer_of(&seller),
    )
    .await
    .expect("fundDeployShell(tc) accepted");

    // Watch the NATIVE balance, not the ECC ledger. `flag: 16` is the whole
    // point of this call and it does not deposit SHELL as SHELL — it converts
    // the credit into the target's native gas, which is what an arriving deploy
    // message needs and what ordinary ECC would not give it. A run that reads
    // `ecc[SHELL]` here sees zero on a funding that worked perfectly.
    let tc_funded = poll_until("the deal address to be funded", || async {
        native_of(&dex, &tc).await >= tc_before.native + TARGET_SHELL
    })
    .await;
    let tc_after = dex.self_rooted_account_shell(&tc).await.expect("deal address after");
    let rm_untouched = dex.self_rooted_account_shell(&root_model).await.expect("rm after tc");
    let note_after_tc = dex.dex_account_shell(&seller.address).await.expect("note after tc").shell;
    eprintln!(
        "[e2e_funding] after tc: note={note_after_tc} deal=(+{} native, {}) root_model=({} native, {})",
        tc_after.native.saturating_sub(tc_before.native),
        tc_after.acc_type,
        rm_untouched.native,
        rm_untouched.acc_type
    );
    if !tc_funded {
        failures.push(format!(
            "the deal address went {} → {} native for a send of {TARGET_SHELL}",
            tc_before.native, tc_after.native
        ));
    }
    if tc_after.native - tc_before.native != TARGET_SHELL {
        failures.push(format!(
            "the deal address gained {}, not the {TARGET_SHELL} it was sent — a flag-16 credit \
             arrives whole or it is not what a deploy can spend",
            tc_after.native.saturating_sub(tc_before.native)
        ));
    }
    // The note has no route to its RootModel through this call any more. An
    // account that so much as comes into existence here means the leg is back.
    if rm_untouched.native != root_model_before.native
        || rm_untouched.acc_type != root_model_before.acc_type
    {
        failures.push(format!(
            "fundDeployShell reached the RootModel, which it has no leg for: {} ({}) → {} ({})",
            root_model_before.native,
            root_model_before.acc_type,
            rm_untouched.native,
            rm_untouched.acc_type
        ));
    }
    if note_before - note_after_tc != TARGET_SHELL {
        failures.push(format!(
            "the note paid {} for a one-target funding of {TARGET_SHELL}",
            note_before.saturating_sub(note_after_tc)
        ));
    }
    if !failures.is_empty() {
        assert!(failures.is_empty(), "e2e_funding failures: {failures:#?}");
    }

    // ── 3. and the same call asked to send nothing ────────────────────────
    //
    // `amount 0 = skip` is what the contract documents, and "skip" has to mean
    // no message at all — not a message carrying nothing, which would still cost
    // the note and, at a fresh address, would leave an account existing and
    // empty. There is no second leg to answer this one with any more, so it is
    // asked directly: run the call with nothing in it and require that both the
    // target and the note's ledger are exactly where the funding call left them.
    dex.fund_deploy_shell(
        &seller.address,
        ParamsOfFundDeployShell { nonce, tc_shell: 0 },
        signer_of(&seller),
    )
    .await
    .expect("fundDeployShell(0) accepted");

    // A skipped send has no arrival to wait for, so there is no polling here —
    // the read is one settle-tick after the call, and the claim is that nothing
    // changed. Sleeping is what makes a later arrival a failure rather than a
    // miss.
    tokio::time::sleep(POLL_TICK).await;
    let tc_unchanged = dex.self_rooted_account_shell(&tc).await.expect("deal after zero call");
    let note_after = dex.dex_account_shell(&seller.address).await.expect("note after").shell;
    eprintln!(
        "[e2e_funding] after zero call: note={note_after} deal=({} native, {})",
        tc_unchanged.native, tc_unchanged.acc_type
    );
    if tc_unchanged.native != tc_after.native {
        failures.push(format!(
            "a tcShell of 0 still moved the deal address: {} → {} native",
            tc_after.native, tc_unchanged.native
        ));
    }
    if note_after != note_after_tc {
        failures
            .push(format!("a tcShell of 0 still debited the note: {note_after_tc} → {note_after}"));
    }
    if !failures.is_empty() {
        assert!(failures.is_empty(), "e2e_funding failures: {failures:#?}");
    }

    // ── 4. and the deploy lands on it ─────────────────────────────────────
    //
    // No giver is involved. If the note had credited the address any other way,
    // this constructor would be accepted and the account would stay uninit.
    let deployed = plan.send(dex.context()).await;
    match deployed {
        Ok(address) => {
            eprintln!("[e2e_funding] deployed {address}");
            match dex.token_contract_get_parties(&tc).await {
                Ok(parties) if parties.seller_note == seller.address => {}
                Ok(parties) => failures.push(format!(
                    "the deal came up naming {} as its seller note, not the note that paid for it \
                     ({})",
                    parties.seller_note, seller.address
                )),
                Err(err) => failures
                    .push(format!("the deal activated but does not answer its getters: {err:?}")),
            }
        }
        Err(err) => failures.push(format!(
            "the constructor never activated the account the note funded: {err:?}. The SHELL \
             arrived, so what failed is the FORM it arrived in — a cross-dApp deploy activates \
             only on an address whose SHELL landed as native gas (flag 16)"
        )),
    }

    assert!(failures.is_empty(), "e2e_funding failures: {failures:#?}");
}

/// Native balance of an address that is the root of its own dApp, readable
/// before anything is deployed there — and the place a `flag: 16` credit lands.
async fn native_of(dex: &Dex, address: &str) -> u128 {
    dex.self_rooted_account_shell(address).await.map(|a| a.native).unwrap_or(0)
}

async fn poll_until<F, Fut>(what: &str, probe: F) -> bool
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..POLL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if probe().await {
            return true;
        }
    }
    eprintln!("[e2e_funding] never reached: {what}");
    false
}
