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
// The call takes no address. Both targets are derived inside the note from its
// own key (`_ephemeralPubkey`), plus the nonce for the deal contract — so SHELL
// sent this way can only ever reach that note's own canonical RootModel and
// `TokenContract`, and never an address a caller picked. The test pins that by
// deriving both addresses independently, from the canonical SuperRoot and from
// the deploy message itself, and checking the money arrived exactly there.
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

/// SHELL for the deal contract. The same amount the giver-funded route sends,
/// so the deploy that follows is asked to work under identical conditions and
/// the funding route is the only thing that differs.
const TC_SHELL: u128 = 200_000_000_000;

/// SHELL for the canonical RootModel. Nothing is deployed there in this run —
/// the point is only that the note reaches its OWN RootModel address and no
/// other — so a nominal amount is enough, and a distinct one at that: two
/// different numbers cannot be swapped between the targets unnoticed.
const ROOT_MODEL_SHELL: u128 = 3_000_000_000;

const _: () = assert!(
    TC_SHELL != ROOT_MODEL_SHELL,
    "the two targets must be told apart by amount, or a call that funded the wrong one would read \
     the same"
);

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

    // ── 1. where the note's money is supposed to go ───────────────────────
    //
    // Both addresses are worked out here, from outside the note: the RootModel
    // from the canonical SuperRoot, the deal contract from the deploy message
    // itself. The note derives the same two internally and is never told
    // either, so an agreement between the two derivations is the only thing
    // that says the SHELL went where it was meant to.
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

    // ── 2. the note pays for both ─────────────────────────────────────────
    dex.fund_deploy_shell(
        &seller.address,
        ParamsOfFundDeployShell { nonce, root_model_shell: ROOT_MODEL_SHELL, tc_shell: TC_SHELL },
        signer_of(&seller),
    )
    .await
    .expect("fundDeployShell accepted");

    // Watch the NATIVE balance, not the ECC ledger. `flag: 16` is the whole
    // point of this call and it does not deposit SHELL as SHELL — it converts
    // the credit into the target's native gas, which is what an arriving deploy
    // message needs and what ordinary ECC would not give it. A run that reads
    // `ecc[SHELL]` here sees zero on a funding that worked perfectly.
    let arrived = poll_until("both deploy targets to be funded", || async {
        native_of(&dex, &root_model).await > root_model_before.native
            && native_of(&dex, &tc).await > tc_before.native
    })
    .await;

    let root_model_after = dex.self_rooted_account_shell(&root_model).await.expect("rm after");
    let tc_after = dex.self_rooted_account_shell(&tc).await.expect("deal address after");
    let note_after = dex.dex_account_shell(&seller.address).await.expect("note after").shell;
    let rm_gain = root_model_after.native.saturating_sub(root_model_before.native);
    let tc_gain = tc_after.native.saturating_sub(tc_before.native);
    eprintln!(
        "[e2e_funding] after: note={note_after} root_model=(+{rm_gain} native, {} ecc, {}) \
         deal=(+{tc_gain} native, {} ecc, {})",
        root_model_after.shell, root_model_after.acc_type, tc_after.shell, tc_after.acc_type
    );
    if !arrived {
        failures.push(format!(
            "the note's SHELL reached neither target as gas: RootModel {} → {} native, deal {} → \
             {} native. The note WAS debited (see below), so this says the money went somewhere \
             else — most likely these two addresses are not the ones the note derived internally",
            root_model_before.native, root_model_after.native, tc_before.native, tc_after.native
        ));
    }
    // The two amounts differ by more than sixty times, so whatever the credit
    // does to a native balance on the way in, a call that swapped the targets
    // lands here. Exact equality is deliberately not asserted: the conversion
    // this credit performs is not something this test has measured, and an
    // assertion resting on a guess about it would be a statement about the
    // guess.
    if arrived && tc_gain <= rm_gain {
        failures.push(format!(
            "the deal address gained {tc_gain} and the RootModel {rm_gain}; the deal was sent \
             {TC_SHELL} against the RootModel's {ROOT_MODEL_SHELL}, so the deal must come out far \
             ahead — reversed, the two targets were swapped"
        ));
    }
    // And it came out of the note, not out of thin air. This one IS exact: the
    // debit is a plain ECC send out of the note's own ledger, whatever happens
    // at the far end.
    if note_before - note_after != ROOT_MODEL_SHELL + TC_SHELL {
        failures.push(format!(
            "the note paid {} for a funding of {}; the note is the source here, so the two have \
             to agree",
            note_before.saturating_sub(note_after),
            ROOT_MODEL_SHELL + TC_SHELL
        ));
    }
    if !failures.is_empty() {
        assert!(failures.is_empty(), "e2e_funding failures: {failures:#?}");
    }

    // ── 3. and the deploy lands on it ─────────────────────────────────────
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
