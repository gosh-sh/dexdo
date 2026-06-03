//! One-shot: deploy a single Oracle on a chosen endpoint and print
//! its identifying fields as JSON to stdout. Default endpoint:
//! `shellnet.ackinacki.org`.
//!
//! Flow:
//!   1. Generate a fresh signing keypair (the "oracle keys").
//!   2. Top up `RootOracle` from giver if its native balance is low.
//!   3. `RootOracle.deployOracle` with the oracle pubkey + chosen name.
//!   4. Wait for the deployed `Oracle` account to go Active.
//!   5. Resolve `EventList` address at index 0 and wait for it Active.
//!
//! Output (stdout, single JSON object):
//!   {
//!     "address":            "<oracle address>",
//!     "name":               "<oracle name>",
//!     "pubkey_hex":         "<64-hex chars, no 0x>",
//!     "secret_hex":         "<64-hex chars, no 0x — SECRET>",
//!     "event_list_address": "<EventList @ index 0>"
//!   }
//!
//! Run: `cargo run --bin deploy_oracle --release`

use std::process::ExitCode;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use ackinacki_kit::contracts::account::AccountStatus;
use ackinacki_kit::contracts::account::ParamsOfWaitAccount;
use ackinacki_kit::contracts::dex::oracle::Oracle;
use ackinacki_kit::contracts::dex::oracle::ParamsOfGetEventListAddress;
use ackinacki_kit::contracts::dex::oracle_event_list::OracleEventList;
use ackinacki_kit::contracts::dex::root_oracle::ParamsOfDeployOracle;
use ackinacki_kit::contracts::dex::root_oracle::RootOracle;
use ackinacki_kit::contracts::giver::v3::top_up_native_with_giver_if_below;
use ackinacki_kit::contracts::traits::AccountAccessor;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use ackinacki_kit::tvm_client::crypto::ParamsOfMnemonicDeriveSignKeys;
use ackinacki_kit::tvm_client::crypto::ParamsOfMnemonicFromRandom;
use ackinacki_kit::tvm_client::ClientConfig;
use ackinacki_kit::tvm_client::ClientContext;
use dodex_sdk::dex_contract_params;
use dodex_sdk::proof;
use dodex_sdk::Dex;
use dodex_sdk::DexConfig;

const DEFAULT_ENDPOINT: &str = "shellnet.ackinacki.org";
const DEFAULT_NAME_PREFIX: &str = "dodex-oracle";
const ROOT_ORACLE_NATIVE_TARGET: u64 = 120_000_000_000;
const ROOT_ORACLE_NATIVE_THRESHOLD: u64 = 50_000_000_000;

struct Args {
    endpoint: String,
    name_prefix: String,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut endpoint = DEFAULT_ENDPOINT.to_string();
        let mut name_prefix = DEFAULT_NAME_PREFIX.to_string();

        let mut argv = std::env::args().skip(1);
        while let Some(arg) = argv.next() {
            match arg.as_str() {
                "--endpoint" | "-e" => {
                    endpoint = argv.next().ok_or("--endpoint requires a value")?;
                }
                "--name-prefix" | "-p" => {
                    name_prefix = argv.next().ok_or("--name-prefix requires a value")?;
                }
                "--help" | "-h" => return Err(usage()),
                other => return Err(format!("unknown arg `{other}`\n\n{}", usage())),
            }
        }

        Ok(Args { endpoint, name_prefix })
    }
}

fn usage() -> String {
    format!(
        "usage: deploy_oracle [--endpoint host] [--name-prefix str]\n\n  \
         --endpoint     network host (default {DEFAULT_ENDPOINT})\n  \
         --name-prefix  oracle name prefix; suffixed with `-<unix_hex>` \
         (default `{DEFAULT_NAME_PREFIX}`)"
    )
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("time").as_secs()
}

fn create_tvm_context(endpoint: &str) -> Arc<ClientContext> {
    let mut config = ClientConfig::default();
    config.network.endpoints = Some(vec![endpoint.to_string()]);
    Arc::new(ClientContext::new(config).expect("create context"))
}

fn gen_keys(context: Arc<ClientContext>) -> KeyPair {
    let phrase = crypto::mnemonic_from_random(
        context.clone(),
        ParamsOfMnemonicFromRandom { dictionary: None, word_count: Some(24) },
    )
    .expect("mnemonic")
    .phrase;
    crypto::mnemonic_derive_sign_keys(
        context,
        ParamsOfMnemonicDeriveSignKeys {
            phrase,
            path: None,
            dictionary: None,
            word_count: Some(24),
        },
    )
    .expect("derive keys")
}

async fn wait_active<T: AccountAccessor>(contract: &T, label: &str) -> Result<(), String> {
    contract
        .wait_account(ParamsOfWaitAccount {
            status: AccountStatus::Active,
            attempts: Some(30),
            attempts_timeout: Some(2_000),
        })
        .await
        .map(|_| ())
        .map_err(|e| format!("wait {label} active: {e:?}"))
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match Args::parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    eprintln!("[deploy_oracle] endpoint = {}", args.endpoint);

    let context = create_tvm_context(&args.endpoint);
    let dex = match Dex::new(DexConfig {
        endpoints: vec![args.endpoint.clone()],
        ..Default::default()
    }) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[deploy_oracle] Dex::new: {e:?}");
            return ExitCode::FAILURE;
        }
    };

    // 1. Fresh oracle keypair + ephemeral deploy-signer keypair.
    let oracle_keys = gen_keys(context.clone());
    let ephemeral_keys = gen_keys(context.clone());

    let run_id = now_unix();
    let oracle_name = format!("{}-{:x}", args.name_prefix, run_id);
    eprintln!("[deploy_oracle] name = {oracle_name}");
    eprintln!("[deploy_oracle] oracle pubkey = {}", oracle_keys.public);

    // 2. Top up RootOracle so it has gas to materialize the Oracle.
    let root_oracle =
        RootOracle::new(context.clone(), dex_contract_params(RootOracle::DEFAULT_ADDRESS));
    if let Err(e) = wait_active(&root_oracle, "RootOracle").await {
        eprintln!("[deploy_oracle] {e}");
        return ExitCode::FAILURE;
    }
    if let Err(e) = top_up_native_with_giver_if_below(
        context.clone(),
        &root_oracle,
        ROOT_ORACLE_NATIVE_TARGET,
        ROOT_ORACLE_NATIVE_THRESHOLD,
        "RootOracle",
    )
    .await
    {
        eprintln!("[deploy_oracle] top up RootOracle: {e:?}");
        return ExitCode::FAILURE;
    }

    // 3. deployOracle (signed by an ephemeral key — the on-chain oracle
    //    pubkey is the one inside ParamsOfDeployOracle).
    if let Err(e) = dex
        .deploy_oracle(
            ParamsOfDeployOracle {
                oracle_pubkey: proof::pubkey_to_dec(&oracle_keys.public),
                oracle_name: oracle_name.clone(),
            },
            Signer::Keys { keys: ephemeral_keys },
        )
        .await
    {
        eprintln!("[deploy_oracle] deploy_oracle: {e:?}");
        return ExitCode::FAILURE;
    }

    // 4. Resolve Oracle address, wait Active.
    let oracle_address = match dex.get_oracle_address(oracle_name.clone()).await {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[deploy_oracle] get_oracle_address: {e:?}");
            return ExitCode::FAILURE;
        }
    };
    let oracle_contract = Oracle::new(context.clone(), dex_contract_params(&oracle_address));
    if let Err(e) = wait_active(&oracle_contract, "Oracle").await {
        eprintln!("[deploy_oracle] {e}");
        return ExitCode::FAILURE;
    }

    // 5. EventList @ index 0, wait Active.
    let event_list_address = match dex
        .get_event_list_address(&oracle_address, ParamsOfGetEventListAddress { index: 0 })
        .await
    {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[deploy_oracle] get_event_list_address: {e:?}");
            return ExitCode::FAILURE;
        }
    };
    let el_contract = OracleEventList::new(context.clone(), dex_contract_params(&event_list_address));
    if let Err(e) = wait_active(&el_contract, "EventList").await {
        eprintln!("[deploy_oracle] {e}");
        return ExitCode::FAILURE;
    }

    let payload = serde_json::json!({
        "address":            oracle_address,
        "name":               oracle_name,
        "pubkey_hex":         oracle_keys.public,
        "secret_hex":         oracle_keys.secret,
        "event_list_address": event_list_address,
        "endpoint":           args.endpoint,
    });

    println!("{}", serde_json::to_string_pretty(&payload).expect("serialize payload"));
    eprintln!("[deploy_oracle] done");
    ExitCode::SUCCESS
}
