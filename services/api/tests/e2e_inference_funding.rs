// A note puts its own deal contract on chain and keeps its gas reserve topped
// up, with no wallet and no giver behind it.
//
// ## What changed, and why this test is not the one it used to be
//
// Until contracts 4.0.36 a deal was deployed off-chain: the seller signed an
// external message carrying the whole contract code, which landed the deal in a
// dApp of its OWN. An unconfigured dApp cannot mint, so every vmshell the deal
// would ever spend had to be sent ahead of it and sized by a published formula
// — undershoot and the deal stopped forever with the escrow inside. That is
// what `fundDeployShell` existed for, and it sent under flag 16 because native
// value does not cross a dApp boundary and only a flag-16 credit becomes the
// new account's native gas.
//
// None of that is true now. `deployDeal` deploys the contract from THIS note as
// an internal message, so the deal lands in the note's dApp, mints its own
// native floor in `ensureBalance`, and carries its gas reserve with it. What it
// cannot mint is ECC[2], and ECC[2] is what every entrypoint burns
// (`_chargeGas`). So `fundDeployShell` survives with the opposite job and the
// opposite flag: it is the deal's ONLY top-up, and it sends under **flag 1** so
// the credit stays SHELL instead of converting to native — the one pocket that
// refills itself.
//
// ## What is asserted
//
//   1. the address is derived from outside the note, and nothing is there yet;
//   2. `deployDeal` brings the deal alive with no giver in the run, naming this
//      note as its seller — the note paid the reserve out of its own SHELL;
//   3. `fundDeployShell` moves exactly its argument, off the note's SHELL and
//      onto the deal's, and reaches nothing else;
//   4. `amount 0 = skip` means no message at all, not a message carrying zero.
//
// Balances are read as the ECC SHELL ledger, not the native balance. That is
// the inverse of the pre-4.0.36 reading and it is the whole point of the flag
// change: a run that watches `native` here sees nothing on a top-up that worked.
//
// ## What the call is NOT allowed to do
//
// It takes no address. The target is derived inside the note from its own key
// (`_ephemeralPubkey`) plus the nonce, so SHELL sent this way can only ever
// reach that note's own canonical deal and never an address a caller picked.
// The test pins that by deriving the address independently and checking the
// money arrived exactly there.
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
use common::airegistry::canonical_deal_address;
use common::airegistry::deal_gas_reserve;
use common::airegistry::deploy_deal_from_note;
use common::airegistry::TokenDeal;
use common::e2e_setup::network_endpoint;
use common::test_pns::TestPn;
use common::test_pns::TestPnPool;
use dodex_chain::Dex;
use dodex_contracts::dex::private_note::ParamsOfFundDeployShell;

const POLL_TICK: Duration = Duration::from_secs(2);
const POLL_TICKS: u32 = 45; // 90s budget per wait.

/// What the top-up sends. Large enough to be unambiguous against the reserve
/// the deploy already put there, and small enough that a run does not move real
/// money around: a top-up is a few terminal charges' worth, not a deposit.
const TOPUP_SHELL: u128 = 500_000_000;

/// Deal terms. Nothing trades here; they exist because the constructor demands
/// a coherent set (`pricePerTick > 0`, `maxTicks >= 2`) and because the address
/// depends on none of them — it comes from the seller key and the nonce alone.
const PRICE_PER_TICK: u128 = 1_000_000_000;
const DEAL_TICKS: u128 = 2;

fn unique_suffix() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

fn keys_of(note: &TestPn) -> KeyPair {
    KeyPair { public: note.owner_public_key_hex.clone(), secret: note.owner_secret_key_hex.clone() }
}

fn signer_of(note: &TestPn) -> Signer {
    Signer::Keys { keys: keys_of(note) }
}

#[tokio::test]
#[ignore = "requires shellnet + seed_notes.json"]
async fn a_note_deploys_and_tops_up_its_own_deal_contract_without_a_wallet() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info,ackinacki_kit=debug")
        .try_init();

    let pool = TestPnPool::load_inference();
    let seller = pool.notes[18 % pool.notes.len()].clone();
    let dex = Dex::from_endpoints(vec![network_endpoint()]).expect("Dex::from_endpoints");
    let suffix = unique_suffix();
    let nonce = (suffix % 1_000_000_000) as u64 + 1;
    let model_name = format!("e2e-funding--{suffix}");
    let deal = TokenDeal {
        model_name: model_name.clone(),
        price_per_tick: PRICE_PER_TICK,
        max_ticks: DEAL_TICKS,
    };
    let reserve = deal_gas_reserve(DEAL_TICKS);
    eprintln!("[e2e_funding] note={} nonce={nonce} reserve={reserve}", seller.address);

    let mut failures: Vec<String> = Vec::new();

    // ── 1. where the note's money is supposed to go ───────────────────────
    //
    // Worked out here, from outside the note. The note derives the same address
    // internally and is never told this one, so an agreement between the two
    // derivations is the only thing that says the SHELL went where it was meant
    // to.
    let tc = canonical_deal_address(
        dex.context(),
        &seller.owner_public_key_hex,
        &seller.address,
        nonce,
        &deal,
        keys_of(&seller),
    )
    .await
    .expect("derive the canonical deal address");
    eprintln!("[e2e_funding] deal={tc}");

    let tc_before = dex.dex_account_shell(&tc).await.expect("deal address before");
    let note_before = dex.dex_account_shell(&seller.address).await.expect("note before").shell;
    eprintln!(
        "[e2e_funding] before: note={note_before} deal=({} shell, {} native, {})",
        tc_before.shell, tc_before.native, tc_before.acc_type
    );
    // The address is derived from a nonce nobody has used, so there must be
    // nothing there. If there were, every delta below would be measuring
    // somebody else's deal.
    if tc_before.acc_type == "Active" {
        panic!(
            "the deal address {tc} already runs code before anything was deployed; the nonce is \
             not fresh and this run would be reading another deal"
        );
    }

    // ── 2. the note deploys it, paying the reserve itself ─────────────────
    //
    // No giver is involved anywhere in this run. `deployDeal` is bounce:false —
    // there is no account at the target yet — so a refusal surfaces only as an
    // address that never activates, which is what the helper's diagnosis is for.
    // This is also where the two derivations are compared. The helper waits for
    // the account at the address computed HERE to go Active; the note computes
    // its own target internally and is never told this one. Nothing comes alive
    // at this address unless they agree, so there is no separate assertion to
    // make — a disagreement is a timeout with the diagnosis attached.
    if let Err(err) = deploy_deal_from_note(
        &dex,
        &seller.address,
        &seller.owner_public_key_hex,
        nonce,
        deal,
        keys_of(&seller),
    )
    .await
    {
        panic!("{err}");
    }

    // The deal must name the note that paid for it. Its constructor refuses any
    // other sender, so a deal answering with a different seller note would mean
    // the address is not the one this note deployed.
    match dex.token_contract_get_parties(&tc).await {
        Ok(parties) if parties.seller_note == seller.address => {}
        Ok(parties) => failures.push(format!(
            "the deal came up naming {} as its seller note, not the note that paid for it ({})",
            parties.seller_note, seller.address
        )),
        Err(err) => {
            failures.push(format!("the deal activated but does not answer its getters: {err:?}"))
        }
    }

    let tc_after_deploy = dex.dex_account_shell(&tc).await.expect("deal after deploy");
    let note_after_deploy =
        dex.dex_account_shell(&seller.address).await.expect("note after deploy").shell;
    eprintln!(
        "[e2e_funding] after deploy: note={note_after_deploy} deal=({} shell, {} native, {})",
        tc_after_deploy.shell, tc_after_deploy.native, tc_after_deploy.acc_type
    );
    // The reserve arrived as ECC[2] and the constructor immediately burned
    // GAS_DEPLOY (0.100) out of it, so the exact figure is not asserted — what
    // is asserted is that a reserve is there at all and that it did not exceed
    // what was sent. A deal with nothing left cannot be closed by its buyer.
    if tc_after_deploy.shell == 0 || tc_after_deploy.shell > reserve {
        failures.push(format!(
            "the deal holds {} SHELL after a deploy that sent it {reserve} as its gas reserve; \
             every entrypoint burns its charge from this pot, so an empty one strands the \
             terminal paths a buyer needs",
            tc_after_deploy.shell
        ));
    }
    if note_before.saturating_sub(note_after_deploy) < reserve {
        failures.push(format!(
            "the note paid {} for a deploy carrying a reserve of {reserve} — the reserve comes \
             off the note's own SHELL, there is no other source in this run",
            note_before.saturating_sub(note_after_deploy)
        ));
    }
    if !failures.is_empty() {
        assert!(failures.is_empty(), "e2e_funding failures: {failures:#?}");
    }

    // ── 3. and the note tops it up ────────────────────────────────────────
    //
    // Watch the ECC SHELL ledger, not the native balance. `flag: 1` is the whole
    // point of this call: it funds the balance in the currency sent, so the
    // credit stays ECC[2] — the pocket `_chargeGas` draws on. A `flag: 16` send
    // would convert it to native and top up the one pocket the deal refills for
    // itself, and a run reading `native` here would call that a success.
    dex.fund_deploy_shell(
        &seller.address,
        ParamsOfFundDeployShell { nonce, tc_shell: TOPUP_SHELL },
        signer_of(&seller),
    )
    .await
    .expect("fundDeployShell(topup) accepted");

    let want = tc_after_deploy.shell + TOPUP_SHELL;
    let topped_up = poll_until("the deal's reserve to grow by the top-up", || async {
        dex.dex_account_shell(&tc).await.map(|a| a.shell >= want).unwrap_or(false)
    })
    .await;
    let tc_after_topup = dex.dex_account_shell(&tc).await.expect("deal after topup");
    let note_after_topup =
        dex.dex_account_shell(&seller.address).await.expect("note after topup").shell;
    eprintln!(
        "[e2e_funding] after topup: note={note_after_topup} deal=(+{} shell)",
        tc_after_topup.shell.saturating_sub(tc_after_deploy.shell)
    );
    if !topped_up {
        failures.push(format!(
            "the deal's SHELL went {} → {} for a top-up of {TOPUP_SHELL}",
            tc_after_deploy.shell, tc_after_topup.shell
        ));
    }
    if tc_after_topup.shell.saturating_sub(tc_after_deploy.shell) != TOPUP_SHELL {
        failures.push(format!(
            "the deal gained {}, not the {TOPUP_SHELL} it was sent — a flag-1 credit arrives whole \
             in the currency it was sent in, or it is not what `_chargeGas` can spend",
            tc_after_topup.shell.saturating_sub(tc_after_deploy.shell)
        ));
    }
    if note_after_deploy.saturating_sub(note_after_topup) != TOPUP_SHELL {
        failures.push(format!(
            "the note paid {} for a top-up of {TOPUP_SHELL}",
            note_after_deploy.saturating_sub(note_after_topup)
        ));
    }
    if !failures.is_empty() {
        assert!(failures.is_empty(), "e2e_funding failures: {failures:#?}");
    }

    // ── 4. and the same call asked to send nothing ────────────────────────
    //
    // `amount 0 = skip` is what the contract documents, and "skip" has to mean
    // no message at all — not a message carrying nothing, which would still cost
    // the note. Ask it directly: run the call with nothing in it and require
    // that both the deal and the note's ledger are exactly where the top-up left
    // them.
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
    let tc_unchanged = dex.dex_account_shell(&tc).await.expect("deal after zero call");
    let note_after = dex.dex_account_shell(&seller.address).await.expect("note after").shell;
    eprintln!(
        "[e2e_funding] after zero call: note={note_after} deal=({} shell)",
        tc_unchanged.shell
    );
    if tc_unchanged.shell != tc_after_topup.shell {
        failures.push(format!(
            "a tcShell of 0 still moved the deal: {} → {} shell",
            tc_after_topup.shell, tc_unchanged.shell
        ));
    }
    if note_after != note_after_topup {
        failures.push(format!(
            "a tcShell of 0 still debited the note: {note_after_topup} → {note_after}"
        ));
    }

    assert!(failures.is_empty(), "e2e_funding failures: {failures:#?}");
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
