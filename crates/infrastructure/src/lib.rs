// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

// The default 128 is not enough to prove `Send` for the future behind
// `ChainOrderSender::submit_order` (`chain_sender.rs`). `#[async_trait]` boxes it
// as `dyn Future + Send`, and proving that auto trait walks the whole chain the
// call opens — `Dex::place_order` -> `PrivateNote::place_order` ->
// `SendMessage::send_message` -> `encode_message` and further into tvm-sdk — one
// coroutine witness per hop. Nightly's `recursion_depth_exceeding_limit`
// future-compat lint reports the overflow instead of silently accepting it
// (rust-lang/rust#159228), and it is a whole-crate lint that cannot be allowed
// per function; the limit is the knob the compiler itself points at. Raising it
// changes nothing at runtime — it is a budget for the trait solver, not a bound
// on anything the code does.
#![recursion_limit = "256"]

pub mod account_registry;
pub mod auth;
pub mod chain_sender;
pub mod config;
pub mod crypto;
pub mod database;
pub mod decoder;
pub mod graphql;
pub mod indexer_repo;
pub mod inference_projectors;
pub mod inference_read_repo;
pub mod inference_reconciler;
pub mod oracle_event_list_reconciler;
pub mod pn_state_reader;
pub mod postgres_repo;
pub mod projectors;
pub mod reconciler;
pub mod rows;
pub mod seed;
pub mod signal;
pub mod token_contract_projectors;
pub mod tvm_hash;
pub mod tvm_runner;
