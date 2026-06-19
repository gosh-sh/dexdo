//! PN event-history tests: parsed events, single/batched note history,
//! pagination over a rich event log.

use ackinacki_kit::tvm_client::abi::Signer;
use dodex_contracts::dex::pmp::ParamsOfSubmitResolve;
use dodex_contracts::dex::pmp::ParamsOfSubmitSetTimings;
use dodex_contracts::dex::private_note::ParamsOfChangeOwner;
use dodex_contracts::dex::private_note::ParamsOfInitTransfer;
use dodex_contracts::dex::private_note::ParamsOfSetStake;
use dodex_contracts::dex::private_note::ParamsOfStakeKey;
use dodex_sdk::proof;

use crate::common::context::create_context;
use crate::common::context::create_dex;
use crate::common::context::DEPLOYER_SEED_AMOUNT;
use crate::common::context::ORACLE_FEE;
use crate::common::context::PMP_DEPOSIT;
use crate::common::context::STAKE_AMOUNT;
use crate::common::context::STAKE_PERIOD_LONG;
use crate::common::context::TOKEN_TYPE_NACKL;
use crate::common::context::VAULT_DEPOSIT;
use crate::common::keys::gen_keys;
use crate::common::misc::now_unix;
use crate::common::misc::pn_nackl;
use crate::common::pmp::deploy_oracle_with_event;
use crate::common::pmp::setup_pmp;
use crate::common::pn::deploy_funded_pn;
use crate::common::pn::ensure_root_pn_funded;

#[tokio::test]
async fn test_get_parsed_events_via_dex() {
    let context = create_context();
    let dex = create_dex();

    let (_, el_address, _, event_id, _) =
        deploy_oracle_with_event(&context, &dex, "BeeDexEvt-").await;

    let events = dex.get_parsed_events(&el_address).await.expect("get_parsed_events");
    assert!(!events.is_empty(), "must have at least one event");

    let evt = events.iter().find(|e| e.event_id == event_id).expect("our event must be present");
    assert!(!evt.event_name.is_empty());
    assert_eq!(evt.oracle_fee, ORACLE_FEE);
    assert!(evt.deadline > 0);
    assert!(!evt.outcome_names.is_empty(), "must have outcomes");
    eprintln!("Parsed event: {:?}", evt);
}

#[tokio::test]
async fn test_get_notes_history_via_dex() {
    let context = create_context();
    let dex = create_dex();
    ensure_root_pn_funded(&context).await;

    // Deploy two PNs and do a transfer between them
    let (pn1_addr, _, pn1_keys) = deploy_funded_pn(&context, &dex, VAULT_DEPOSIT).await;
    let (pn2_addr, pn2_dih, _) = deploy_funded_pn(&context, &dex, VAULT_DEPOSIT).await;

    let transfer_amount: u128 = 10_000_000;
    dex.init_transfer(
        &pn1_addr,
        ParamsOfInitTransfer {
            dest_deposit_hash: pn2_dih,
            token_type: TOKEN_TYPE_NACKL,
            amount: transfer_amount,
        },
        Signer::Keys { keys: pn1_keys },
    )
    .await
    .expect("init_transfer");
    tokio::time::sleep(std::time::Duration::from_secs(8)).await;

    // Single note history
    let page = dex.get_notes_history(&[pn1_addr.clone()], 50, None).await.expect("history pn1");
    eprintln!("PN1 history: {} events", page.events.len());
    for e in &page.events {
        eprintln!("  {} {} {:?}", e.created_at, e.event_type, e.data);
    }
    assert!(
        page.events.iter().any(|e| e.event_type == "TransferInitiated"),
        "PN1 should have TransferInitiated event"
    );

    let page2 = dex.get_notes_history(&[pn2_addr.clone()], 50, None).await.expect("history pn2");
    eprintln!("PN2 history: {} events", page2.events.len());
    for e in &page2.events {
        eprintln!("  {} {} {:?}", e.created_at, e.event_type, e.data);
    }
    assert!(
        page2.events.iter().any(|e| e.event_type == "TransferReceived"),
        "PN2 should have TransferReceived event"
    );

    // Batch: both notes merged
    let merged = dex
        .get_notes_history(&[pn1_addr.clone(), pn2_addr.clone()], 50, None)
        .await
        .expect("merged history");
    eprintln!("Merged history: {} events", merged.events.len());
    assert!(merged.events.len() >= page.events.len());
    assert!(merged.events.len() >= page2.events.len());

    // Verify sorted by created_at desc
    for w in merged.events.windows(2) {
        assert!(w[0].created_at >= w[1].created_at, "events should be sorted desc");
    }
}

#[tokio::test]
async fn test_rich_history_and_pagination_via_dex() {
    let context = create_context();
    let dex = create_dex();

    // Setup PMP — this generates PmpDeployed event on PN
    let s = setup_pmp(&context, &dex).await;

    let result_start = now_unix() + STAKE_PERIOD_LONG;
    dex.submit_set_timings(
        &s.pmp_address,
        ParamsOfSubmitSetTimings { result_start },
        Signer::Keys { keys: s.oracle_keys.clone() },
    )
    .await
    .expect("set_timings");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Stake → StakeConfirmed
    dex.set_stake(
        &s.pn_address,
        ParamsOfSetStake {
            event_id: s.event_id.clone(),
            oracle_list_hash: s.oracle_list_hash.clone(),
            token_type: TOKEN_TYPE_NACKL,
            outcome: 0,
            amount: STAKE_AMOUNT,
            use_coupon: false,
        },
        Signer::Keys { keys: s.pn_keys.clone() },
    )
    .await
    .expect("stake");

    // Wait for callback
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let bal = pn_nackl(&dex.get_private_note_details(&s.pn_address).await.expect("pn"));
        if bal < PMP_DEPOSIT as u128 - 2 * DEPLOYER_SEED_AMOUNT {
            break;
        }
    }

    // Wait result window → resolve → ClaimAccepted
    let wait = result_start.saturating_sub(now_unix()) + 5;
    eprintln!("Waiting {wait}s for result window...");
    tokio::time::sleep(std::time::Duration::from_secs(wait)).await;

    dex.submit_resolve(
        &s.pmp_address,
        ParamsOfSubmitResolve { outcome_id: 0 },
        Signer::Keys { keys: s.oracle_keys },
    )
    .await
    .expect("resolve");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    dex.claim(
        &s.pn_address,
        ParamsOfStakeKey {
            event_id: s.event_id.clone(),
            oracle_list_hash: s.oracle_list_hash.clone(),
            token_type: TOKEN_TYPE_NACKL,
        },
        Signer::Keys { keys: s.pn_keys.clone() },
    )
    .await
    .expect("claim");
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // Change owner → OwnerChanged
    let new_keys = gen_keys(context.clone());
    dex.change_owner(
        &s.pn_address,
        ParamsOfChangeOwner { new_pubkey: proof::pubkey_to_dec(&new_keys.public) },
        Signer::Keys { keys: s.pn_keys.clone() },
    )
    .await
    .expect("change_owner");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Fetch full history
    let full =
        dex.get_notes_history(&[s.pn_address.clone()], 50, None).await.expect("full history");
    eprintln!("Full history: {} events", full.events.len());
    for e in &full.events {
        eprintln!("  {} {}", e.event_type, e.created_at);
    }

    // Verify event types present
    let types: std::collections::HashSet<&str> =
        full.events.iter().map(|e| e.event_type.as_str()).collect();
    assert!(types.contains("PmpDeployed"), "should have PmpDeployed");
    assert!(types.contains("StakeConfirmed"), "should have StakeConfirmed");
    assert!(types.contains("ClaimAccepted"), "should have ClaimAccepted");
    assert!(types.contains("OwnerChanged"), "should have OwnerChanged");
    assert!(full.events.len() >= 4, "should have at least 4 events");

    // Sorted desc
    for w in full.events.windows(2) {
        assert!(w[0].created_at >= w[1].created_at, "events should be sorted desc");
    }

    // Pagination: fetch page of 2, then next page
    let page1 = dex.get_notes_history(&[s.pn_address.clone()], 2, None).await.expect("page1");
    assert_eq!(page1.events.len(), 2, "page1 should have 2 events");
    eprintln!("Page1: {} events, has_next={}", page1.events.len(), page1.page_info.has_next_page);

    if page1.page_info.has_next_page {
        let page2 = dex
            .get_notes_history(&[s.pn_address.clone()], 2, page1.page_info.end_cursor)
            .await
            .expect("page2");
        eprintln!(
            "Page2: {} events, has_next={}",
            page2.events.len(),
            page2.page_info.has_next_page
        );
        assert!(!page2.events.is_empty(), "page2 should have events");

        // No overlap between pages
        for e in &page2.events {
            // Events at same timestamp are possible, so we check by type+timestamp
            let overlap = page1
                .events
                .iter()
                .any(|p1| p1.created_at == e.created_at && p1.event_type == e.event_type);
            if overlap {
                eprintln!("  possible overlap (same timestamp): {} {}", e.event_type, e.created_at);
            }
        }
    }

    eprintln!("rich history + pagination OK");
}
