//! End-user PrivateNote onboarding via a pre-deployed multisig wallet.
//!
//! Counterpart to `mint_pn_pool` — that one bypasses the user's wallet and
//! mints from the giver; this one drives the **production / user-facing**
//! flow described in `sdk/README.md`:
//!
//!   1. Halo2 deposit voucher minted via `multisig.submitTransaction →
//!      RootPN.generateVoucher(isFee=false)`.
//!   2. `RootPN.deployPrivateNote` — chain materializes the PN, signed by
//!      the freshly generated PN owner keypair.
//!   3. Halo2 SHELL gas voucher via `multisig.submitTransaction →
//!      RootPN.generateVoucher(isFee=true)`. Pre-condition: caller has
//!      already topped the multisig up with enough SHELL ECC (currency 2)
//!      using `flag=1` (so it lands in `balance_other[2]`, not native).
//!      The onboarding skill funds 1000 SHELL post-deploy, which covers
//!      the deposit voucher (Step 6.1) + the gas voucher here + a trading
//!      reserve.
//!   4. `RootPN.sendEccShellToPrivateNote` funds the PN's gas balance.
//!   5. Sanity check via `PrivateNote.getDetails`.
//!
//! Outputs two files (both hold a secret — keep private):
//! - `pn_state.json` (`--output`) — resume state: `pn_address`, `dih_dec`,
//!   the PN owner keypair (**secret!**), step-by-step funding flags.
//!   Re-running is **idempotent at the step boundary** — if the file already
//!   has `pn_address` set, step 1 is skipped; if `shell_funded`, step 3 is
//!   skipped. Not loadable into the API as-is.
//! - `<token>.account.json` (next to `--output`) — the `POST /api/v1/accounts`
//!   request body (`pnAddress`/`pnPubkeyHex`/`pnSeckeyHex`/`pnDihHex`, DIH in
//!   hex), written once the PN is fully onboarded, for later loading into the
//!   API service.
//!
//! Run:
//!
//!   cargo run --release --bin onboard_user_shellnet -- \
//!     --multisig-address 0:<...> \
//!     --multisig-keys-file Multisig.keys.json \
//!     --nominal N100 \
//!     --token-type nackl \
//!     --endpoint shellnet.ackinacki.org \
//!     --output pn_state.json
//!
//! Pre-conditions the tool does NOT do for you:
//! - Multisig wallet already deployed (see Acki Nacki tooling — `tvm-cli`
//!   + the `Multisig` ABI/TVC).
//! - Multisig funded with: (a) enough native vmshell to cover queued
//!   message gas, (b) enough of `--token-type` ECC currency to cover
//!   `--nominal` deposit, (c) ~0.1 SHELL ECC for the gas voucher.
//!
//! Output file holds a secret — keep it private.

use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use ackinacki_kit::contracts::traits::AccountAccessor;
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
use dodex_sdk::halo2::multisig_voucher::mint_voucher_via_multisig;
use dodex_sdk::halo2::Halo2Paths;
use dodex_sdk::proof;
use serde::Deserialize;
use serde::Serialize;

/// ECC currency id for SHELL (gas token — always SHELL regardless of
/// deposit currency).
const CURRENCY_ID_SHELL: u32 = 2;
/// SHELL gas voucher amount — covers oracle fees + per-message gas for
/// typical operations. Matches `mint_pn_pool`'s budget.
const ECC_SHELL_DEPOSIT_RAW: u64 = 100_000_000_000;

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
    multisig_address: String,
    multisig_keys_file: PathBuf,
    nominal: NominalArg,
    token_type: TokenTypeArg,
    endpoint: String,
    output: PathBuf,
    /// `https://{endpoint}` form for halo2 stage A (witness export).
    network_url: String,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut multisig_address: Option<String> = None;
        let mut multisig_keys_file: Option<PathBuf> = None;
        let mut nominal = NominalArg::N100;
        let mut token_type = TokenTypeArg::Nackl;
        let mut endpoint = "shellnet.ackinacki.org".to_string();
        let mut output = PathBuf::from("pn_state.json");

        let mut argv = std::env::args().skip(1);
        while let Some(arg) = argv.next() {
            match arg.as_str() {
                "--multisig-address" => {
                    multisig_address =
                        Some(argv.next().ok_or("--multisig-address requires a value")?);
                }
                "--multisig-keys-file" => {
                    multisig_keys_file = Some(PathBuf::from(
                        argv.next().ok_or("--multisig-keys-file requires a value")?,
                    ));
                }
                "--nominal" => {
                    let v = argv.next().ok_or("--nominal requires a value")?;
                    nominal = NominalArg::parse(&v)?;
                }
                "--token-type" | "-t" => {
                    let v = argv.next().ok_or("--token-type requires a value")?;
                    token_type = TokenTypeArg::parse(&v)?;
                }
                "--endpoint" | "-e" => {
                    endpoint = argv.next().ok_or("--endpoint requires a value")?;
                }
                "--output" | "-o" => {
                    output = PathBuf::from(argv.next().ok_or("--output requires a value")?);
                }
                "--help" | "-h" => return Err(usage()),
                other => return Err(format!("unknown arg `{other}`\n\n{}", usage())),
            }
        }

        let multisig_address = multisig_address.ok_or("--multisig-address is required")?;
        let multisig_keys_file = multisig_keys_file.ok_or("--multisig-keys-file is required")?;

        let network_url = if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
            endpoint.clone()
        } else {
            format!("https://{endpoint}")
        };

        Ok(Args {
            multisig_address,
            multisig_keys_file,
            nominal,
            token_type,
            endpoint,
            output,
            network_url,
        })
    }
}

fn usage() -> String {
    "usage: onboard_user_shellnet --multisig-address 0:<addr> --multisig-keys-file <path> \
         [--nominal N100|N1000|N10000] [--token-type nackl|shell|usdc] \
         [--endpoint host] [--output path]\n\n  \
         --multisig-address    deployed multisig wallet address (required)\n  \
         --multisig-keys-file  JSON keypair from `tvm-cli genphrase --dump` (required)\n  \
         --nominal             PN deposit nominal (default N100)\n  \
         --token-type          deposit currency (default nackl)\n  \
         --endpoint            network host (default shellnet.ackinacki.org)\n  \
         --output              JSON output path (default ./pn_state.json)"
        .to_string()
}

/// On-disk state for a single onboarded PrivateNote. Persisted after every
/// successful step so re-runs can resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PnState {
    endpoint: String,
    multisig_address: String,
    nominal: String,
    token_type: u32,
    raw_value: u64,
    ecc_shell_deposit: u64,
    /// PN address, set after `deployPrivateNote` succeeds.
    pn_address: Option<String>,
    /// Deposit identifier hash (decimal uint256 string), set after step 1.
    deposit_identifier_hash: Option<String>,
    /// PN owner public key (hex, no `0x`), set when keys are generated.
    owner_public_key_hex: Option<String>,
    /// PN owner secret key (hex, no `0x`). **Secret** — protect this file.
    owner_secret_key_hex: Option<String>,
    deployed_at_unix: Option<u64>,
    /// `true` once `sendEccShellToPrivateNote` succeeded.
    shell_funded: bool,
    /// `true` once `PrivateNote.getDetails` returned a coherent reply.
    sanity_checked: bool,
}

/// The `POST /api/v1/accounts` request body (also the api seeder's per-note
/// shape). Written per fully-onboarded PN so it can later be loaded into the
/// API service. Carries the secret key — keep private. Distinct from
/// `PnState`, which is the binary's resume state and is not API-loadable.
#[derive(Debug, Serialize)]
struct AccountFile {
    #[serde(rename = "pnAddress")]
    pn_address: String,
    #[serde(rename = "pnPubkeyHex")]
    pn_pubkey_hex: String,
    #[serde(rename = "pnSeckeyHex")]
    pn_seckey_hex: String,
    #[serde(rename = "pnDihHex")]
    pn_dih_hex: String,
}

/// Decimal uint256 string -> bare hex (no `0x`). The API wants `pnDihHex` in
/// hex, but `PnState` stores the DIH in decimal, so convert on export (also
/// covers the resume path, where only the decimal form survives).
fn dec_u256_to_hex(dec: &str) -> String {
    num_bigint::BigUint::parse_bytes(dec.as_bytes(), 10)
        .expect("deposit_identifier_hash is a valid decimal uint")
        .to_str_radix(16)
}

/// Write the API-registration file next to the state file, named by token
/// (e.g. `shell.account.json`). Called once the PN is fully onboarded.
fn write_account_file(state: &PnState, token_type: TokenTypeArg, state_path: &Path) {
    let account = AccountFile {
        pn_address: state.pn_address.clone().expect("pn_address must be set"),
        pn_pubkey_hex: state.owner_public_key_hex.clone().expect("pn pubkey must be set"),
        pn_seckey_hex: state.owner_secret_key_hex.clone().expect("pn seckey must be set"),
        pn_dih_hex: dec_u256_to_hex(
            state.deposit_identifier_hash.as_deref().expect("dih must be set"),
        ),
    };
    let dir =
        state_path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let path = dir.join(format!("{}.account.json", token_type.label().to_ascii_lowercase()));
    let json = serde_json::to_string_pretty(&account).expect("serialize account file");
    write_secret_file(&path, &json);
    eprintln!("[onboard] API-registration file: {}", path.display());
}

/// Write a file that holds the PN's secret key with owner-only perms (0600).
/// These files (`<tt>.account.json`, `pn_state.<tt>.json`) carry `pnSeckeyHex`,
/// so they must not be world-readable like a default `fs::write` (0644) leaves them.
fn write_secret_file(path: &Path, json: &str) {
    use std::io::Write;
    // Create with 0600 from the start (no world-readable window), and also chmod
    // before writing any bytes so a pre-existing looser file is locked down while
    // still empty — the secret never lands on a 0644 file.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .expect("open secret file");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .expect("chmod 600 secret file");
    f.write_all(json.as_bytes()).expect("write secret file");
}

#[derive(Debug, Clone, Deserialize)]
struct MultisigKeys {
    public: String,
    secret: String,
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
    config.network.max_reconnect_timeout = 0;
    Arc::new(ClientContext::new(config).expect("create tvm client"))
}

fn save_state(state: &PnState, path: &Path) {
    let json = serde_json::to_string_pretty(state).expect("serialize state");
    // 0600 — the state file holds the note's secret key (owner_secret_key_hex).
    write_secret_file(path, &json);
}

fn load_or_init_state(path: &Path, args: &Args) -> Result<PnState, String> {
    let want_tt = args.token_type.id();
    let want_raw = args.nominal.raw_value(args.token_type);
    let want_nominal = args.nominal.label().to_string();

    if !path.exists() {
        return Ok(PnState {
            endpoint: args.endpoint.clone(),
            multisig_address: args.multisig_address.clone(),
            nominal: want_nominal,
            token_type: want_tt,
            raw_value: want_raw,
            ecc_shell_deposit: ECC_SHELL_DEPOSIT_RAW,
            pn_address: None,
            deposit_identifier_hash: None,
            owner_public_key_hex: None,
            owner_secret_key_hex: None,
            deployed_at_unix: None,
            shell_funded: false,
            sanity_checked: false,
        });
    }

    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let existing: PnState = serde_json::from_slice(&bytes)
        .map_err(|e| format!("parse {} as PnState: {e}", path.display()))?;

    // Compatibility check — re-running with different shape would silently
    // produce inconsistent state. Force a fresh `--output` for that case.
    if existing.endpoint != args.endpoint {
        return Err(format!(
            "existing state endpoint `{}` != requested `{}`; use a fresh --output",
            existing.endpoint, args.endpoint,
        ));
    }
    if existing.multisig_address != args.multisig_address {
        return Err(format!(
            "existing state multisig_address `{}` != requested `{}`",
            existing.multisig_address, args.multisig_address,
        ));
    }
    if existing.nominal != want_nominal {
        return Err(format!(
            "existing state nominal `{}` != requested `{}`",
            existing.nominal, want_nominal,
        ));
    }
    if existing.token_type != want_tt {
        return Err(format!(
            "existing state token_type `{}` != requested `{want_tt}` ({})",
            existing.token_type,
            args.token_type.label(),
        ));
    }
    if existing.raw_value != want_raw {
        return Err(format!(
            "existing state raw_value `{}` != requested `{}`",
            existing.raw_value, want_raw,
        ));
    }
    Ok(existing)
}

fn load_multisig_keys(path: &Path) -> Result<KeyPair, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("read multisig keys {}: {e}", path.display()))?;
    let parsed: MultisigKeys = serde_json::from_slice(&bytes)
        .map_err(|e| format!("parse {} as multisig keys: {e}", path.display()))?;
    // tvm-cli writes hex without `0x`, but some toolchains add it. Strip
    // it to match `KeyPair`'s expected format.
    Ok(KeyPair {
        public: proof::strip_0x(&parsed.public).to_string(),
        secret: proof::strip_0x(&parsed.secret).to_string(),
    })
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
        "[onboard] {} on {} via multisig {} (nominal={}, raw={})",
        args.token_type.label(),
        args.endpoint,
        args.multisig_address,
        args.nominal.label(),
        args.nominal.raw_value(args.token_type),
    );
    eprintln!("[onboard] state file: {}", args.output.display());

    let multisig_keys = match load_multisig_keys(&args.multisig_keys_file) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("[onboard] {e}");
            return ExitCode::FAILURE;
        }
    };

    let context = create_tvm_context(&args.endpoint);
    let paths = Halo2Paths::from_env();
    if !paths.srs_exists() {
        eprintln!(
            "[onboard] SRS {} not found — generating it (~64 MB, one-time, CPU-bound)…",
            paths.srs_path().display()
        );
        paths.ensure_srs();
    }
    if let Err(e) = paths.validate() {
        eprintln!("[onboard] halo2 paths invalid: {e:?}");
        return ExitCode::FAILURE;
    }

    let mut state = match load_or_init_state(&args.output, &args) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[onboard] {e}");
            return ExitCode::FAILURE;
        }
    };
    save_state(&state, &args.output);

    let root_pn = RootPn::new(context.clone(), dex_contract_params(RootPn::DEFAULT_ADDRESS));

    // STEP 1: deposit voucher + deployPrivateNote (skipped if pn_address
    // already persisted from a prior run).
    if state.pn_address.is_none() {
        eprintln!("\n[onboard] step 1/3 — deposit voucher + deployPrivateNote");

        let pn_keys = match generate_random_sign_keys(context.clone()) {
            Ok(k) => k,
            Err(e) => {
                eprintln!("[onboard] generate_random_sign_keys: {e:?}");
                return ExitCode::FAILURE;
            }
        };
        // Persist keys before submitting so a halo2 / chain crash mid-step
        // doesn't lose the secret we'll need on retry.
        state.owner_public_key_hex = Some(pn_keys.public.clone());
        state.owner_secret_key_hex = Some(pn_keys.secret.clone());
        save_state(&state, &args.output);

        eprintln!("    halo2 {} deposit voucher (slow)…", args.token_type.label());
        let deposit_zk = match mint_voucher_via_multisig(
            context.clone(),
            args.network_url.clone(),
            &args.multisig_address,
            multisig_keys.clone(),
            &pn_keys.public,
            args.token_type.id(),
            args.nominal.raw_value(args.token_type),
            false,
            &paths,
        )
        .await
        {
            Ok(zk) => zk,
            Err(e) => {
                eprintln!("[onboard] mint_voucher_via_multisig (deposit): {e:?}");
                return ExitCode::FAILURE;
            }
        };

        let dih_dec = proof::hex_u256_to_dec(&deposit_zk.deposit_identifier_hash_hex);
        let epk_dec = proof::pubkey_to_dec(&pn_keys.public);

        eprintln!("    RootPN.deployPrivateNote…");
        if let Err(e) = root_pn
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
                Signer::Keys { keys: pn_keys.clone() },
            )
            .await
        {
            eprintln!("[onboard] deploy_private_note: {e:?}");
            return ExitCode::FAILURE;
        }

        let pn_address = match root_pn
            .get_private_note_address(ParamsOfGetPrivateNoteAddress {
                deposit_identifier_hash: dih_dec.clone(),
            })
            .await
        {
            Ok(r) => r.private_note_address,
            Err(e) => {
                eprintln!("[onboard] get_private_note_address: {e:?}");
                return ExitCode::FAILURE;
            }
        };

        let pn = PrivateNote::new(context.clone(), dex_contract_params(&pn_address));
        eprintln!("    waiting for PN {pn_address} to be Active…");
        if let Err(e) = pn
            .wait_account(ackinacki_kit::contracts::account::ParamsOfWaitAccount {
                status: ackinacki_kit::contracts::account::AccountStatus::Active,
                attempts: Some(60),
                attempts_timeout: Some(2_000),
            })
            .await
        {
            eprintln!("[onboard] wait PN active: {e:?}");
            return ExitCode::FAILURE;
        }

        state.pn_address = Some(pn_address);
        state.deposit_identifier_hash = Some(dih_dec);
        state.deployed_at_unix = Some(now_unix());
        save_state(&state, &args.output);
        eprintln!("    PN deployed.");
    } else {
        eprintln!("\n[onboard] step 1/3 — skipped (pn_address already set)");
    }

    let pn_address = state.pn_address.clone().expect("pn_address must be set by now");
    let dih_dec = state.deposit_identifier_hash.clone().expect("dih_dec must be set by now");
    let pn_keys = KeyPair {
        public: state.owner_public_key_hex.clone().expect("pn pubkey must be set"),
        secret: state.owner_secret_key_hex.clone().expect("pn seckey must be set"),
    };

    // STEP 2: SHELL gas voucher + sendEccShellToPrivateNote (skipped if
    // already funded).
    if !state.shell_funded {
        eprintln!("\n[onboard] step 2/3 — SHELL gas voucher + sendEccShellToPrivateNote");

        eprintln!("    halo2 SHELL gas voucher (via multisig.submitTransaction)…");
        let gas_zk = match mint_voucher_via_multisig(
            context.clone(),
            args.network_url.clone(),
            &args.multisig_address,
            multisig_keys.clone(),
            &pn_keys.public,
            CURRENCY_ID_SHELL,
            ECC_SHELL_DEPOSIT_RAW,
            true,
            &paths,
        )
        .await
        {
            Ok(zk) => zk,
            Err(e) => {
                eprintln!("[onboard] mint_voucher_via_multisig (gas): {e:?}");
                return ExitCode::FAILURE;
            }
        };

        eprintln!("    RootPN.sendEccShellToPrivateNote…");
        if let Err(e) = root_pn
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
                    recipient_ephemeral_pubkey: proof::pubkey_to_dec(&pn_keys.public),
                },
                Signer::Keys { keys: pn_keys.clone() },
            )
            .await
        {
            eprintln!("[onboard] send_ecc_shell_to_private_note: {e:?}");
            return ExitCode::FAILURE;
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        state.shell_funded = true;
        save_state(&state, &args.output);
        eprintln!("    PN gas funded.");
    } else {
        eprintln!("\n[onboard] step 2/3 — skipped (shell_funded already true)");
    }

    // STEP 3: sanity check — round-trip via PrivateNote.getDetails.
    if !state.sanity_checked {
        eprintln!("\n[onboard] step 3/3 — sanity check (PrivateNote.getDetails)");
        let pn = PrivateNote::new(context.clone(), dex_contract_params(&pn_address));
        match pn.get_details().await {
            Ok(details) => {
                eprintln!(
                    "    PN OK: address={} dih={} balance={:?}",
                    pn_address, dih_dec, details.balance,
                );
                state.sanity_checked = true;
                save_state(&state, &args.output);
            }
            Err(e) => {
                eprintln!("[onboard] PrivateNote.getDetails: {e:?}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        eprintln!("\n[onboard] step 3/3 — skipped (sanity_checked already true)");
    }

    write_account_file(&state, args.token_type, &args.output);

    eprintln!(
        "\n[onboard] DONE — PN ready at {}; state saved to {}",
        pn_address,
        args.output.display(),
    );
    ExitCode::SUCCESS
}
