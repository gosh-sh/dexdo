//! Pre-deploy a pool of PrivateNotes at a fixed nominal and persist the
//! result as JSON. Designed for orderbook scenarios that need many large
//! funded PNs ready before the test/script starts (avoids paying the
//! halo2 cost in the hot path).
//!
//! Each PN goes through the full production-grade flow:
//!
//!   1. Halo2 NACKL voucher proof (deposit) via `mint_voucher_via_giver`.
//!   2. `RootPN.deployPrivateNote` — chain materializes the PN.
//!   3. Halo2 SHELL voucher proof (gas) via `mint_voucher_via_giver`.
//!   4. `RootPN.sendEccShellToPrivateNote` — funds the PN's gas.
//!   5. Giver-side native vmshell top-up so the PN can run internal ops.
//!
//! Sequential by design — halo2 voucher proofs commit to the chain's
//! `final_layer_historical_hash_root` at proof generation time and the
//! contract verifies it against current chain state at submit. Parallel
//! mints don't fit the validity window.
//!
//! Output: `pn_pool.json` (default; override with `--output`) appended
//! incrementally so Ctrl-C / errors mid-run don't lose what already
//! deployed. Re-running against an existing output file is **additive**:
//! the existing notes are loaded, validated for endpoint/nominal/token_type
//! compatibility, and `--count` more notes are appended on top. Use
//! `--output` to start a fresh pool.
//!
//! Run: `cargo run --bin mint_pn_pool --release -- --count 5 --nominal N10000`
//! (release recommended — halo2 prover is CPU-bound).
//!
//! Output schema (see `Pool` / `PoolNote` below) — includes owner
//! public/secret keypair so downstream code can sign on behalf of the PN
//! owner. **The JSON contains secrets**; treat the file as private.

use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use ackinacki_kit::contracts::giver::v3::send_currency_with_flag_from_default_giver;
use ackinacki_kit::contracts::giver::v3::top_up_native_with_giver_if_below;
use ackinacki_kit::contracts::traits::AccountAccessor;
use ackinacki_kit::contracts::traits::AddressAccessor;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::generate_random_sign_keys;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use ackinacki_kit::tvm_client::ClientConfig;
use ackinacki_kit::tvm_client::ClientContext;
use dodex_contracts::dex::private_note::PrivateNote;
use dodex_contracts::dex::root_pn::ParamsOfDeployPrivateNote;
use dodex_contracts::dex::root_pn::ParamsOfGetPrivateNoteAddress;
use dodex_contracts::dex::root_pn::ParamsOfSendEccShellToPrivateNote;
use dodex_contracts::dex::root_pn::RootPn;
use dodex_sdk::dex_contract_params;
use dodex_sdk::halo2::giver_voucher::mint_voucher_via_giver;
use dodex_sdk::halo2::Halo2Paths;
use dodex_sdk::maybe_acquire;
use dodex_sdk::proof;
use dodex_sdk::RateLimiter;
use serde::Deserialize;
use serde::Serialize;

/// ECC currency id for SHELL (gas token — always SHELL regardless of
/// deposit currency).
const CURRENCY_ID_SHELL: u32 = 2;
/// SHELL gas voucher amount per PN — covers oracle fees + per-message gas
/// for typical orderbook ops.
const ECC_SHELL_DEPOSIT_RAW: u64 = 100_000_000_000;

/// Deposit currency for the PN pool. Mirrors `proof::TokenType` but kept
/// local so the CLI doesn't pull in the full enum surface.
#[derive(Debug, Clone, Copy)]
enum TokenTypeArg {
    Nackl,
    Shell,
    Usdc,
}

impl TokenTypeArg {
    fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "nackl" | "1" => Ok(Self::Nackl),
            "shell" | "2" => Ok(Self::Shell),
            "usdc" | "3" => Ok(Self::Usdc),
            other => Err(format!("unknown token-type `{other}` (use nackl|shell|usdc)")),
        }
    }

    fn id(self) -> u32 {
        match self {
            Self::Nackl => 1,
            Self::Shell => 2,
            Self::Usdc => 3,
        }
    }

    /// One whole token in raw units. NACKL/SHELL = 1e9, USDC = 1e6.
    fn decimals(self) -> u64 {
        match self {
            Self::Usdc => 1_000_000,
            _ => 1_000_000_000,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Nackl => "NACKL",
            Self::Shell => "SHELL",
            Self::Usdc => "USDC",
        }
    }
}
/// Native vmshell top-up per PN — fuels the PN's internal-message
/// execution (separate from SHELL ECC gas).
const NATIVE_GAS_TOPUP_RAW: u64 = 20_000_000_000;
/// RootPN minimum native balance + top-up amount before we start minting
/// vouchers in bulk. Mirrors `dodex_sdk/tests/integration/common/pn.rs`.
const ROOTPN_NATIVE_MIN: u64 = 120_000_000_000;
const ROOTPN_NATIVE_TOPUP: u64 = 50_000_000_000;
/// SHELL ECC budget kept on RootPN — each `sendEccShellToPrivateNote`
/// burns some, so we top up generously before the run.
const ROOTPN_SHELL_BUDGET: u64 = 1_000_000_000_000;

#[derive(Debug, Clone, Copy)]
enum NominalArg {
    N100,
    N1000,
    N10000,
}

impl NominalArg {
    fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "n100" | "100" => Ok(Self::N100),
            "n1000" | "1000" => Ok(Self::N1000),
            "n10000" | "10000" => Ok(Self::N10000),
            other => Err(format!("unknown nominal `{other}` (use N100|N1000|N10000)")),
        }
    }

    fn count(self) -> u64 {
        match self {
            Self::N100 => 100,
            Self::N1000 => 1_000,
            Self::N10000 => 10_000,
        }
    }

    fn raw_value(self, token_type: TokenTypeArg) -> u64 {
        self.count() * token_type.decimals()
    }

    fn label(self) -> &'static str {
        match self {
            Self::N100 => "N100",
            Self::N1000 => "N1000",
            Self::N10000 => "N10000",
        }
    }
}

#[derive(Debug)]
struct Args {
    count: usize,
    output: PathBuf,
    endpoint: String,
    nominal: NominalArg,
    token_type: TokenTypeArg,
    /// #65: SHELL gas-voucher amount per PN, raw nano (`--deposit-shells N` → `N × 1e9`). Defaults to
    /// `ECC_SHELL_DEPOSIT_RAW`. This is the note's deploy-funding ECC[2] SHELL budget — sizes how many
    /// `TokenContract`/deals the note serves before re-funding.
    ecc_shell_deposit_raw: u64,
    /// `https://{endpoint}` form for halo2 stage A (witness export).
    network_url: String,
    /// Cap on outbound TVM requests per second. `None` disables pacing.
    /// The shellnet BM rejects bursts above ~3 rps with HTTP 429, so the
    /// default keeps a single run under that ceiling.
    max_rps: Option<u32>,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut count: usize = 5;
        let mut output = PathBuf::from("pn_pool.json");
        let mut endpoint = "shellnet.ackinacki.org".to_string();
        let mut nominal = NominalArg::N10000;
        let mut token_type = TokenTypeArg::Nackl;
        let mut max_rps: Option<u32> = Some(3);
        let mut ecc_shell_deposit_raw = ECC_SHELL_DEPOSIT_RAW;

        let mut argv = std::env::args().skip(1);
        while let Some(arg) = argv.next() {
            match arg.as_str() {
                "--count" | "-n" => {
                    let v = argv.next().ok_or("--count requires a value")?;
                    count = v.parse().map_err(|e| format!("--count: {e}"))?;
                    if count == 0 {
                        return Err("--count must be ≥ 1".into());
                    }
                }
                "--output" | "-o" => {
                    output = PathBuf::from(argv.next().ok_or("--output requires a value")?);
                }
                "--endpoint" | "-e" => {
                    endpoint = argv.next().ok_or("--endpoint requires a value")?;
                }
                "--nominal" => {
                    let v = argv.next().ok_or("--nominal requires a value")?;
                    nominal = NominalArg::parse(&v)?;
                }
                "--token-type" | "-t" => {
                    let v = argv.next().ok_or("--token-type requires a value")?;
                    token_type = TokenTypeArg::parse(&v)?;
                }
                "--max-rps" => {
                    let v = argv.next().ok_or("--max-rps requires a value")?;
                    let rps: u32 = v.parse().map_err(|e| format!("--max-rps: {e}"))?;
                    max_rps = (rps > 0).then_some(rps);
                }
                "--deposit-shells" => {
                    // #65: note SHELL gas budget in vmshell → raw nano. Fail-closed:
                    // non-zero + overflow-checked (this funds live on-chain gas).
                    let v = argv.next().ok_or("--deposit-shells requires a value")?;
                    let n: u64 = v.parse().map_err(|e| format!("--deposit-shells: {e}"))?;
                    if n == 0 {
                        return Err("--deposit-shells must be ≥ 1 vmshell".into());
                    }
                    ecc_shell_deposit_raw = n
                        .checked_mul(1_000_000_000)
                        .ok_or("--deposit-shells: overflows u64 raw nano range")?;
                }
                "--help" | "-h" => return Err(usage()),
                other => return Err(format!("unknown arg `{other}`\n\n{}", usage())),
            }
        }

        let network_url = if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
            endpoint.clone()
        } else {
            format!("https://{endpoint}")
        };

        Ok(Args {
            count,
            output,
            endpoint,
            nominal,
            token_type,
            ecc_shell_deposit_raw,
            network_url,
            max_rps,
        })
    }
}

fn usage() -> String {
    "usage: mint_pn_pool [--count N] [--output path] [--endpoint host] \
         [--nominal N100|N1000|N10000] [--token-type nackl|shell|usdc] [--deposit-shells N] [--max-rps N]\n\n  \
         --count          number of PrivateNotes to deploy (default 5)\n  \
         --output         JSON output path (default ./pn_pool.json)\n  \
         --endpoint       network host (default shellnet.ackinacki.org)\n  \
         --nominal        PN deposit nominal (default N10000)\n  \
         --token-type     deposit currency (default nackl)\n  \
         --deposit-shells note SHELL gas budget in vmshell (default 100; #65 — fund generously for many deals)\n  \
         --max-rps        cap outbound TVM requests/sec (default 3; 0 disables)\n\n  \
         Also writes <output>.seed_notes.json (api seeder / e2e format) beside the pool."
        .to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pool {
    endpoint: String,
    created_at_unix: u64,
    nominal: String,
    token_type: u32,
    raw_value_per_pn: u64,
    ecc_shell_deposit_per_pn: u64,
    notes: Vec<PoolNote>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PoolNote {
    address: String,
    /// `depositIdentifierHash` as decimal uint256 string (the format the
    /// kit/contract APIs accept directly).
    deposit_identifier_hash: String,
    /// PN owner public key (hex, no `0x`).
    owner_public_key_hex: String,
    /// PN owner secret key (hex, no `0x`). **Secret** — keep the JSON file
    /// out of public stores.
    owner_secret_key_hex: String,
    deployed_at_unix: u64,
    /// `true` once `sendEccShellToPrivateNote` succeeded.
    shell_funded: bool,
    /// `true` once the giver native top-up succeeded.
    native_funded: bool,
}

/// One note in the seed_notes format the api seeder (`seed_accounts_from_notes`)
/// and the e2e loader consume. The pool stores `deposit_identifier_hash` as
/// decimal; here it is hex (`pn_dih_hex`).
#[derive(Debug, Serialize)]
struct SeedNote {
    pn_address: String,
    pn_pubkey_hex: String,
    pn_seckey_hex: String,
    pn_dih_hex: String,
    #[serde(rename = "tokenType")]
    token_type: u32,
    value: u64,
}

fn pool_to_seed_notes(pool: &Pool) -> Vec<SeedNote> {
    pool.notes
        .iter()
        .map(|n| SeedNote {
            pn_address: n.address.clone(),
            pn_pubkey_hex: n.owner_public_key_hex.clone(),
            pn_seckey_hex: n.owner_secret_key_hex.clone(),
            pn_dih_hex: num_bigint::BigUint::parse_bytes(n.deposit_identifier_hash.as_bytes(), 10)
                .expect("deposit_identifier_hash is decimal uint256")
                .to_str_radix(16),
            token_type: pool.token_type,
            value: pool.raw_value_per_pn,
        })
        .collect()
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs()
}

fn create_tvm_context(endpoint: &str) -> Arc<ClientContext> {
    let mut config = ClientConfig::default();
    config.network.endpoints = Some(vec![endpoint.to_string()]);
    // Match production: disable tvm_client's internal re-connect loop;
    // we don't have a retry policy layered around bin calls, but at
    // least we avoid turning a flaky shellnet into a self-amplifying storm.
    config.network.max_reconnect_timeout = 0;
    Arc::new(ClientContext::new(config).expect("create tvm client"))
}

fn save_pool(pool: &Pool, path: &Path) {
    let json = serde_json::to_string_pretty(pool).expect("serialize pool");
    std::fs::write(path, json).expect("write pool json");

    // Also emit the seed_notes format the api seeder / e2e loader read, as a
    // sidecar next to the pool. The pool file stays the resumable source (and
    // the shape `mint_ob_pool --deployer-pn-pool` expects).
    let seed_path = seed_notes_sidecar(path);
    let seed_json =
        serde_json::to_string_pretty(&pool_to_seed_notes(pool)).expect("serialize seed_notes");
    std::fs::write(&seed_path, seed_json).expect("write seed_notes json");
}

/// `foo.json` -> `foo.seed_notes.json` (sidecar path for the API format).
fn seed_notes_sidecar(path: &Path) -> PathBuf {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("pool");
    path.parent().unwrap_or_else(|| Path::new(".")).join(format!("{stem}.seed_notes.json"))
}

/// Load an existing pool from `path` if it exists, validating that its
/// endpoint / nominal / token_type / raw_value match what this run would
/// produce. Otherwise initialise a fresh empty pool.
///
/// Compatibility check exists because `--count` is additive: appending
/// notes from a different nominal or endpoint into the same file would
/// silently corrupt the pool's invariants (callers assume all notes share
/// the same shape).
fn load_or_init_pool(path: &Path, args: &Args) -> Result<Pool, String> {
    let want_tt = args.token_type.id();
    if !path.exists() {
        return Ok(Pool {
            endpoint: args.endpoint.clone(),
            created_at_unix: now_unix(),
            nominal: args.nominal.label().to_string(),
            token_type: want_tt,
            raw_value_per_pn: args.nominal.raw_value(args.token_type),
            ecc_shell_deposit_per_pn: args.ecc_shell_deposit_raw,
            notes: Vec::with_capacity(args.count),
        });
    }

    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let existing: Pool = serde_json::from_slice(&bytes)
        .map_err(|e| format!("parse {} as Pool: {e}", path.display()))?;

    let want_nominal = args.nominal.label();
    let want_raw = args.nominal.raw_value(args.token_type);
    if existing.endpoint != args.endpoint {
        return Err(format!(
            "existing pool endpoint `{}` != requested `{}`; pass --output to a fresh path",
            existing.endpoint, args.endpoint,
        ));
    }
    if existing.nominal != want_nominal {
        return Err(format!(
            "existing pool nominal `{}` != requested `{}`; pools are nominal-homogeneous, \
             pass --output to a fresh path for a different nominal",
            existing.nominal, want_nominal,
        ));
    }
    if existing.token_type != want_tt {
        return Err(format!(
            "existing pool token_type `{}` != requested `{want_tt}` ({}); \
             pools are token-homogeneous, pass --output to a fresh path",
            existing.token_type,
            args.token_type.label(),
        ));
    }
    if existing.raw_value_per_pn != want_raw {
        return Err(format!(
            "existing pool raw_value_per_pn `{}` != requested `{}`",
            existing.raw_value_per_pn, want_raw,
        ));
    }
    if existing.ecc_shell_deposit_per_pn != args.ecc_shell_deposit_raw {
        return Err(format!(
            "existing pool ecc_shell_deposit_per_pn `{}` != requested `{}` (--deposit-shells); \
             pass --output to a fresh path",
            existing.ecc_shell_deposit_per_pn, args.ecc_shell_deposit_raw,
        ));
    }
    Ok(existing)
}

async fn ensure_root_pn_funded(
    context: &Arc<ClientContext>,
    rl: Option<&RateLimiter>,
) -> Result<(), String> {
    let root_pn = RootPn::new(context.clone(), dex_contract_params(RootPn::DEFAULT_ADDRESS));
    eprintln!("[pool] waiting for RootPN ({}) to be Active…", root_pn.address());
    maybe_acquire(rl).await;
    root_pn
        .wait_account(ackinacki_kit::contracts::account::ParamsOfWaitAccount {
            status: ackinacki_kit::contracts::account::AccountStatus::Active,
            attempts: Some(60),
            attempts_timeout: Some(2_000),
        })
        .await
        .map_err(|e| format!("wait RootPN active: {e:?}"))?;

    eprintln!("[pool] topping up RootPN native gas…");
    maybe_acquire(rl).await;
    top_up_native_with_giver_if_below(
        context.clone(),
        &root_pn,
        ROOTPN_NATIVE_MIN,
        ROOTPN_NATIVE_TOPUP,
        "RootPN",
    )
    .await
    .map_err(|e| format!("top_up_native_with_giver_if_below RootPN: {e:?}"))?;

    eprintln!("[pool] sending SHELL ECC budget to RootPN…");
    let mut ecc = std::collections::HashMap::new();
    ecc.insert(CURRENCY_ID_SHELL, ROOTPN_SHELL_BUDGET);
    maybe_acquire(rl).await;
    send_currency_with_flag_from_default_giver(
        context.clone(),
        RootPn::DEFAULT_ADDRESS,
        50_000_000_000,
        ecc,
        1,
    )
    .await
    .map_err(|e| format!("giver SHELL → RootPN: {e:?}"))?;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    Ok(())
}

async fn deploy_one_pn(
    context: Arc<ClientContext>,
    network_url: &str,
    paths: &Halo2Paths,
    token_type: TokenTypeArg,
    nominal_raw: u64,
    ecc_shell_deposit_raw: u64,
    keys: KeyPair,
    rl: Option<&RateLimiter>,
) -> Result<PoolNote, String> {
    let root_pn = RootPn::new(context.clone(), dex_contract_params(RootPn::DEFAULT_ADDRESS));

    // 1. Halo2 deposit voucher in the chosen currency.
    eprintln!("    halo2 {} deposit voucher (this is the slow step)…", token_type.label());
    maybe_acquire(rl).await;
    let deposit_zk = mint_voucher_via_giver(
        context.clone(),
        network_url.to_string(),
        &keys.public,
        token_type.id(),
        nominal_raw,
        false,
        paths,
    )
    .await
    .map_err(|e| format!("mint_voucher_via_giver (deposit): {e:?}"))?;

    let dih_dec = proof::hex_u256_to_dec(&deposit_zk.deposit_identifier_hash_hex);
    let epk_dec = proof::pubkey_to_dec(&keys.public);

    // 2. Deploy PN against the deposit proof.
    eprintln!("    RootPN.deployPrivateNote…");
    maybe_acquire(rl).await;
    root_pn
        .deploy_private_note(
            ParamsOfDeployPrivateNote {
                zkproof: deposit_zk.proof,
                deposit_identifier_hash: dih_dec.clone(),
                final_layer_historical_hash_root: proof::hex_u256_to_dec(
                    &deposit_zk.final_layer_historical_hash_root_hex,
                ),
                voucher_nominal_fr: proof::hex_u256_to_dec(&deposit_zk.voucher_nominal_fr_hex),
                token_type_fr: proof::hex_u256_to_dec(&deposit_zk.token_type_fr_hex),
                ephemeral_pubkey: epk_dec,
                value: deposit_zk.voucher_value,
                token_type: deposit_zk.voucher_token_type,
                layer_number: deposit_zk.layer_number,
            },
            Signer::Keys { keys: keys.clone() },
        )
        .await
        .map_err(|e| format!("deploy_private_note: {e:?}"))?;

    maybe_acquire(rl).await;
    let pn_address = root_pn
        .get_private_note_address(ParamsOfGetPrivateNoteAddress {
            deposit_identifier_hash: dih_dec.clone(),
        })
        .await
        .map_err(|e| format!("get_private_note_address: {e:?}"))?
        .private_note_address;

    let pn = PrivateNote::new(context.clone(), dex_contract_params(&pn_address));
    eprintln!("    waiting for PN {pn_address} to be Active…");
    maybe_acquire(rl).await;
    pn.wait_account(ackinacki_kit::contracts::account::ParamsOfWaitAccount {
        status: ackinacki_kit::contracts::account::AccountStatus::Active,
        attempts: Some(60),
        attempts_timeout: Some(2_000),
    })
    .await
    .map_err(|e| format!("wait PN active: {e:?}"))?;

    let mut note = PoolNote {
        address: pn_address.clone(),
        deposit_identifier_hash: dih_dec.clone(),
        owner_public_key_hex: keys.public.clone(),
        owner_secret_key_hex: keys.secret.clone(),
        deployed_at_unix: now_unix(),
        shell_funded: false,
        native_funded: false,
    };

    // 3. Halo2 SHELL gas voucher (sequentially, NOT in parallel — see header
    //    comment).
    eprintln!("    halo2 SHELL gas voucher…");
    maybe_acquire(rl).await;
    let gas_zk = mint_voucher_via_giver(
        context.clone(),
        network_url.to_string(),
        &keys.public,
        CURRENCY_ID_SHELL,
        ecc_shell_deposit_raw,
        true,
        paths,
    )
    .await
    .map_err(|e| format!("mint_voucher_via_giver (gas): {e:?}"))?;

    eprintln!("    RootPN.sendEccShellToPrivateNote…");
    maybe_acquire(rl).await;
    root_pn
        .send_ecc_shell_to_private_note(
            ParamsOfSendEccShellToPrivateNote {
                proof: gas_zk.proof,
                nullifier_hash: proof::hex_u256_to_dec(&gas_zk.deposit_identifier_hash_hex),
                deposit_identifier_hash: dih_dec.clone(),
                final_layer_historical_hash_root: proof::hex_u256_to_dec(
                    &gas_zk.final_layer_historical_hash_root_hex,
                ),
                voucher_nominal_fr: proof::hex_u256_to_dec(&gas_zk.voucher_nominal_fr_hex),
                token_type_fr: proof::hex_u256_to_dec(&gas_zk.token_type_fr_hex),
                value: gas_zk.voucher_value,
                layer_number: gas_zk.layer_number,
                recipient_ephemeral_pubkey: proof::pubkey_to_dec(&keys.public),
            },
            Signer::Keys { keys: keys.clone() },
        )
        .await
        .map_err(|e| format!("send_ecc_shell_to_private_note: {e:?}"))?;
    note.shell_funded = true;

    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // 4. Native gas top-up so the PN can execute internal messages.
    eprintln!("    giver native gas top-up…");
    maybe_acquire(rl).await;
    send_currency_with_flag_from_default_giver(
        context.clone(),
        &pn_address,
        NATIVE_GAS_TOPUP_RAW,
        std::collections::HashMap::new(),
        1,
    )
    .await
    .map_err(|e| format!("giver native top-up: {e:?}"))?;
    note.native_funded = true;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    Ok(note)
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

    eprintln!(
        "[pool] minting {} PN(s) at {} ({}, raw={}) on {}",
        args.count,
        args.nominal.label(),
        args.token_type.label(),
        args.nominal.raw_value(args.token_type),
        args.endpoint,
    );
    eprintln!("[pool] writing to {}", args.output.display());

    let context = create_tvm_context(&args.endpoint);
    let rate_limiter = RateLimiter::optional(args.max_rps);
    match args.max_rps {
        Some(rps) => eprintln!("[pool] rate limit: {rps} req/s"),
        None => eprintln!("[pool] rate limit: disabled"),
    }
    let paths = Halo2Paths::from_env();
    if !paths.srs_exists() {
        eprintln!(
            "[pool] SRS {} not found — generating it (~64 MB, one-time, CPU-bound)...",
            paths.srs_path().display()
        );
        paths.ensure_srs();
        eprintln!("[pool] SRS ready at {}", paths.srs_path().display());
    }
    if let Err(e) = paths.validate() {
        eprintln!("[pool] halo2 paths invalid: {e:?}");
        return ExitCode::FAILURE;
    }

    if let Err(e) = ensure_root_pn_funded(&context, rate_limiter.as_ref()).await {
        eprintln!("[pool] failed to fund RootPN: {e}");
        return ExitCode::FAILURE;
    }

    let mut pool = match load_or_init_pool(&args.output, &args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[pool] {e}");
            return ExitCode::FAILURE;
        }
    };
    let starting_count = pool.notes.len();
    if starting_count > 0 {
        eprintln!(
            "[pool] loaded {} existing note(s) from {}; will append {} more (target {})",
            starting_count,
            args.output.display(),
            args.count,
            starting_count + args.count,
        );
    }
    pool.notes.reserve(args.count);
    save_pool(&pool, &args.output);

    let started = std::time::Instant::now();
    for i in 0..args.count {
        let t = std::time::Instant::now();
        eprintln!("\n[pool] === note {}/{} ===", i + 1, args.count);
        let keys = match generate_random_sign_keys(context.clone()) {
            Ok(k) => k,
            Err(e) => {
                eprintln!("[pool] generate_random_sign_keys: {e:?}");
                return ExitCode::FAILURE;
            }
        };

        match deploy_one_pn(
            context.clone(),
            &args.network_url,
            &paths,
            args.token_type,
            args.nominal.raw_value(args.token_type),
            args.ecc_shell_deposit_raw,
            keys,
            rate_limiter.as_ref(),
        )
        .await
        {
            Ok(note) => {
                eprintln!(
                    "[pool] note {}/{} ready at {} in {:.1}s (cumulative {:.1}s)",
                    i + 1,
                    args.count,
                    note.address,
                    t.elapsed().as_secs_f64(),
                    started.elapsed().as_secs_f64(),
                );
                pool.notes.push(note);
                save_pool(&pool, &args.output);
            }
            Err(e) => {
                eprintln!(
                    "[pool] note {}/{} FAILED after {:.1}s: {e}",
                    i + 1,
                    args.count,
                    t.elapsed().as_secs_f64(),
                );
                eprintln!(
                    "[pool] partial pool ({} notes) saved to {}",
                    pool.notes.len(),
                    args.output.display()
                );
                return ExitCode::FAILURE;
            }
        }
    }

    eprintln!(
        "\n[pool] DONE — {} new note(s) deployed in {:.1}s; pool now has {} total, saved to {}",
        pool.notes.len() - starting_count,
        started.elapsed().as_secs_f64(),
        pool.notes.len(),
        args.output.display(),
    );
    ExitCode::SUCCESS
}
