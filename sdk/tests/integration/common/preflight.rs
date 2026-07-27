//! Preflight — the harness's refusal to run against a stand it cannot
//! identify.
//!
//! Everything else here assumes the network under test is running the
//! contracts we think it is. This module turns that assumption into an
//! assertion: it compares the deployed bytecode, the ABIs compiled into our
//! own binaries, and the pre-baked accounts against a manifest produced at
//! the moment the contracts were compiled, and fails the whole run when any
//! of them disagree. A conservation proof run against the wrong contracts
//! proves nothing — and, worse, looks like it proved something.
//!
//! The manifest is located by the `E2E_MANIFEST` environment variable; the
//! shipped TVC images it pins are resolved relative to that file's own
//! directory, because manifest and images travel together.
//!
//! [`run_preflight`] performs, in order:
//!
//! 1. provenance closure — `manifest.dodex_sha` against the `DEXDO_SHA` the
//!    host checkout was made from;
//! 2. semantic ABI hash of every ABI compiled into the contract wrappers;
//! 3. raw code hash of the deployed roots (RootPN, RootOracle, SuperRoot,
//!    ModelRegistry);
//! 4. per seed note — its raw code hash, the address RootPN derives from its
//!    deposit-identifier hash, the keyring closure (public key derived from the
//!    secret, against both the seed file and the note's own
//!    `_ephemeralPubkey`), a clean sweep verdict, and the child code hashes and
//!    depths baked into its storage;
//! 5. `RootPN._deployedValues` against the summed note balances, with RootPN's
//!    physical ECC holding at least that much.
//!
//! Three encoding facts this module depends on, each documented at the code
//! that relies on it, because each one is a thing a later maintainer would
//! "simplify" straight back into a defect:
//!
//! - canonical JSON has to be hand-rolled here ([`canonical_json`]);
//! - a `uint256` decodes very differently from narrower widths
//!   ([`normalize_u256_hex`]);
//! - the code salt is a wrapper cell, not the raw code cell ([`salt_wrapper`]).

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use ackinacki_kit::tvm_client::boc::encode_boc;
use ackinacki_kit::tvm_client::boc::get_boc_depth;
use ackinacki_kit::tvm_client::boc::get_boc_hash;
use ackinacki_kit::tvm_client::boc::get_code_from_tvc;
use ackinacki_kit::tvm_client::boc::set_code_salt;
use ackinacki_kit::tvm_client::boc::BuilderOp;
use ackinacki_kit::tvm_client::boc::ParamsOfEncodeBoc;
use ackinacki_kit::tvm_client::boc::ParamsOfGetBocDepth;
use ackinacki_kit::tvm_client::boc::ParamsOfGetBocHash;
use ackinacki_kit::tvm_client::boc::ParamsOfGetCodeFromTvc;
use ackinacki_kit::tvm_client::boc::ParamsOfSetCodeSalt;
use ackinacki_kit::tvm_client::crypto::nacl_sign_keypair_from_secret_key;
use ackinacki_kit::tvm_client::crypto::ParamsOfNaclSignKeyPairFromSecret;
use ackinacki_kit::tvm_client::ClientConfig;
use ackinacki_kit::tvm_client::ClientContext;
use anyhow::anyhow;
use anyhow::Context as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use dodex_contracts::dex::root_oracle::RootOracle;
use dodex_contracts::dex::root_pn::ParamsOfGetPrivateNoteAddress;
use dodex_contracts::dex::root_pn::RootPn;
use dodex_infrastructure::pn_state_reader::owner_pubkey_from_value;
use dodex_infrastructure::tvm_runner::deployed_code_hash;
use dodex_sdk::dex_contract_params;
use dodex_sdk::proof::strip_0x;
use serde::Deserialize;
use serde_json::Value;
use sha2::Digest as _;
use sha2::Sha256;

use crate::common::allocator::load_seed_notes;
use crate::common::allocator::seed_path;
use crate::common::allocator::sweep_verdict;
use crate::common::allocator::SeedNote;
use crate::common::allocator::SweepVerdict;
use crate::common::chain_reader::ChainReader;

/// Environment variable naming the build-time manifest to verify against.
pub const MANIFEST_ENV: &str = "E2E_MANIFEST";

/// Environment variable carrying the dodex commit the host checkout was made
/// from. The manifest records the same commit at compile time; comparing them
/// is what rules out a manifest that belongs to some other build.
pub const DODEX_SHA_ENV: &str = "DEXDO_SHA";

/// Every ABI compiled into the contract wrappers, paired with the name the
/// manifest knows it by. Closed against `crates/contracts`' actual
/// `include_str!` set by a test below.
///
/// These are the same files `crates/contracts` embeds, included here from the
/// same paths — so equality with the manifest is equality with the bytes our
/// binaries decode storage through. `ModelRegistry` is deliberately absent:
/// it has no Rust wrapper, so nothing in our binaries decodes with it, and
/// the manifest carries it only for the zerostate generator's benefit.
const EMBEDDED_ABIS: &[(&str, &str)] = &[
    (
        "InferenceOrderBook",
        include_str!("../../../../contracts/airegistry/InferenceOrderBook.abi.json"),
    ),
    ("Nullifier", include_str!("../../../../contracts/dex/Nullifier.abi.json")),
    ("Oracle", include_str!("../../../../contracts/dex/Oracle.abi.json")),
    ("OracleEventList", include_str!("../../../../contracts/dex/OracleEventList.abi.json")),
    ("OrderBook", include_str!("../../../../contracts/dex/OrderBook.abi.json")),
    ("PMP", include_str!("../../../../contracts/dex/PMP.abi.json")),
    ("PrivateNote", include_str!("../../../../contracts/dex/PrivateNote.abi.json")),
    ("RootModel", include_str!("../../../../contracts/airegistry/RootModel.abi.json")),
    ("RootOracle", include_str!("../../../../contracts/dex/RootOracle.abi.json")),
    ("RootPN", include_str!("../../../../contracts/dex/RootPN.abi.json")),
    ("SuperRoot", include_str!("../../../../contracts/airegistry/SuperRoot.abi.json")),
    ("TokenContract", include_str!("../../../../contracts/airegistry/TokenContract.abi.json")),
];

const ROOT_PN_ABI: &str = include_str!("../../../../contracts/dex/RootPN.abi.json");
const PRIVATE_NOTE_ABI: &str = include_str!("../../../../contracts/dex/PrivateNote.abi.json");

/// Child codes a `PrivateNote` carries as whole cells. The decoder emits a
/// `cell` field as a base64 BOC, so its repr-hash is directly comparable with
/// the manifest's code hash.
const NOTE_CODE_CELL_FIELDS: &[(&str, &str)] = &[
    ("_privateNoteCode", "PrivateNote"),
    ("_pmpCode", "PMP"),
    ("_orderBookCode", "OrderBook"),
    ("_inferenceOrderBookCode", "InferenceOrderBook"),
];

/// Child codes a `PrivateNote` carries as a (hash, depth) pair rather than a
/// whole cell. Both halves are compared: the depth participates in deriving
/// child addresses, so a correct hash under a wrong depth still yields wrong
/// addresses and must not pass.
const NOTE_CODE_HASH_DEPTH_FIELDS: &[(&str, &str, &str)] = &[
    ("_oracleCodeHash", "_oracleCodeDepth", "Oracle"),
    ("_oracleEventListCodeHash", "_oracleEventListCodeDepth", "OracleEventList"),
    ("_tokenContractCodeHash", "_tokenContractCodeDepth", "TokenContract"),
    ("_rootModelCodeHash", "_rootModelCodeDepth", "RootModel"),
];

/// Keys of [`Manifest::deployed_addresses`] for the two force-placed AI roots
/// the zerostate generator pins at vanity addresses, paired with their
/// manifest contract names. Neither is reachable any other way: `ModelRegistry`
/// has no Rust wrapper at all, and `SuperRoot`'s wrapper carries no
/// `DEFAULT_ADDRESS`.
const VANITY_ROOTS: &[(&str, &str)] =
    &[("super_root", "SuperRoot"), ("model_registry", "ModelRegistry")];

/// The build-time provenance record: what was compiled, from which sources,
/// with which toolchain, onto which runtime images.
#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub dodex_sha: String,
    pub acki_sha: String,
    pub sold_ver: String,
    pub sold_old_ver: String,
    pub tvm_cli_ver: String,
    /// Runtime image references, each an `image@sha256:…` digest.
    pub images: BTreeMap<String, String>,
    pub contracts: Vec<ManifestContract>,
    /// Vanity addresses of the force-placed contracts of the pinned
    /// `generate_zerostate.py` — see [`VANITY_ROOTS`].
    pub deployed_addresses: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct ManifestContract {
    pub name: String,
    pub code_hash: String,
    pub code_depth: u16,
    pub abi_sem_hash: String,
    /// Set for the images that ship alongside the manifest (PrivateNote, PMP,
    /// OrderBook), whose exact bytes the salted-code derivation needs; `None`
    /// for everything else.
    pub tvc_rel_path: Option<String>,
}

impl Manifest {
    /// The entry for `name`, or an error naming what was looked for — a
    /// manifest missing a contract is a broken manifest, never a check to
    /// skip.
    pub fn contract(&self, name: &str) -> anyhow::Result<&ManifestContract> {
        self.contracts
            .iter()
            .find(|c| c.name == name)
            .ok_or_else(|| anyhow!("manifest has no contract named `{name}`"))
    }
}

/// Path to the manifest, from [`MANIFEST_ENV`]. There is no default: a run
/// with no manifest cannot verify anything, and silently proceeding would
/// defeat the entire point of this module.
#[allow(dead_code)] // consumed by the live scenario tests
pub fn manifest_path() -> anyhow::Result<PathBuf> {
    let raw = std::env::var(MANIFEST_ENV)
        .map_err(|_| anyhow!("manifest path not set: export {MANIFEST_ENV}"))?;
    anyhow::ensure!(!raw.is_empty(), "{MANIFEST_ENV} is set but empty");
    Ok(PathBuf::from(raw))
}

/// Directory the manifest lives in — where the shipped TVC images sit too.
#[allow(dead_code)] // consumed by the live scenario tests
pub fn manifest_dir() -> anyhow::Result<PathBuf> {
    let path = manifest_path()?;
    path.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow!("manifest path {} has no parent directory", path.display()))
}

#[allow(dead_code)] // consumed by the live scenario tests
pub fn load_manifest() -> anyhow::Result<Manifest> {
    let path = manifest_path()?;
    let bytes =
        std::fs::read(&path).with_context(|| format!("read manifest {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse manifest {}", path.display()))
}

/// Canonical-JSON digest of an ABI: `sha256` over the same bytes Python's
/// `json.dumps(obj, sort_keys=True, separators=(",", ":"))` produces for the
/// same document. The manifest side of every ABI comparison is generated by
/// exactly that Python call, so the two encodings have to agree byte for
/// byte — see [`canonical_json`] for why the encoding is hand-rolled.
pub fn abi_semantic_hash(abi_json: &str) -> anyhow::Result<String> {
    let value: Value = serde_json::from_str(abi_json).context("parse abi json")?;
    let mut canon = String::new();
    canonical_json(&value, &mut canon)?;
    Ok(hex::encode(Sha256::digest(canon.as_bytes())))
}

/// Append `v` to `out` in canonical form: object keys sorted, no insignificant
/// whitespace, strings escaped the way Python's `json.dumps` escapes them.
///
/// This cannot delegate to `serde_json::Value::to_string()`. The sdk lockfile
/// enables `serde_json/preserve_order` transitively through `tvm_block_json`,
/// and feature unification applies that to the whole build — so `Value`'s map
/// is insertion-ordered here, and `to_string()` emits keys in whatever order
/// the document happened to list them. Reordering an ABI's keys would then
/// change its digest even though the ABI is identical, which is precisely the
/// difference a *semantic* hash exists to ignore.
///
/// Sorting is bytewise, which for UTF-8 is the same order Python's
/// `sort_keys=True` produces (UTF-8 preserves code-point ordering).
fn canonical_json(v: &Value, out: &mut String) -> anyhow::Result<()> {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                escape_json_string(key, out);
                out.push(':');
                canonical_json(&map[*key], out)?;
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical_json(item, out)?;
            }
            out.push(']');
        }
        Value::String(s) => escape_json_string(s, out),
        Value::Number(n) => {
            // Python and serde_json format floats differently, so a float
            // would digest differently on the two sides without either one
            // noticing. An ABI contains no floats; one appearing means the
            // input is not the document this function is for. (An integer too
            // large for `u64` also arrives here as an f64, and is rejected for
            // the same reason.)
            anyhow::ensure!(!n.is_f64(), "canonical json: non-integer number `{n}` in abi");
            out.push_str(&n.to_string());
        }
        Value::Bool(_) | Value::Null => out.push_str(&v.to_string()),
    }
    Ok(())
}

/// Append `s` as a canonical JSON string literal.
///
/// Python's `json.dumps` defaults to `ensure_ascii=True`: every character
/// outside printable ASCII is emitted as `\uXXXX` (a surrogate pair above the
/// BMP). serde_json emits those characters raw, so relying on it would make
/// the two sides disagree on the first ABI containing a non-ASCII comment —
/// and preflight would then reject a perfectly correct stand.
fn escape_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ' '..='~' => out.push(c),
            _ => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

/// The single normalization every 256-bit value passes through before it is
/// compared with anything: strip an optional `0x`, lower-case, and require
/// exactly 64 hex characters.
///
/// The three producers disagree on spelling. `tvm_abi` decodes a `uint256` as
/// `"0x"` + 64 hex digits, while every narrower width decodes as a decimal
/// string (`detokenize_big_uint` branches on the width). The zerostate
/// generator bakes embedded hashes in *with* a `0x`. `tvm-cli`, and therefore
/// the manifest, writes them *without*. A comparison that skips this rejects
/// a correct stand; one that merely strips the prefix without checking the
/// length accepts a truncated value as if it were padded.
pub fn normalize_u256_hex(raw: &str) -> anyhow::Result<String> {
    let digits = strip_0x(raw);
    anyhow::ensure!(
        digits.len() == 64 && digits.bytes().all(|b| b.is_ascii_hexdigit()),
        "`{raw}` is not a 256-bit hex value (want 64 hex digits, optionally 0x-prefixed)"
    );
    Ok(digits.to_ascii_lowercase())
}

/// A decoded `map(uint32, uint128)` storage field, as a strict map.
///
/// Deliberately stricter than the allocator's ledger bookkeeping, which
/// tolerates entries it cannot read: a conservation comparison built on a
/// silently shortened sum produces a wrong answer instead of a loud failure.
/// A missing field is an error for the same reason the sweep lists are
/// fail-closed — it means the ABI no longer describes the deployed contract.
fn uint_map(fields: &Value, name: &str) -> anyhow::Result<BTreeMap<u32, u128>> {
    let obj = fields
        .get(name)
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("storage field `{name}` is missing or not a map"))?;
    obj.iter()
        .map(|(k, v)| {
            let key: u32 = k.parse().with_context(|| format!("`{name}` key `{k}`"))?;
            let raw = v.as_str().ok_or_else(|| anyhow!("`{name}[{k}]` is not a string"))?;
            let amount: u128 =
                raw.parse().with_context(|| format!("`{name}[{k}]` value `{raw}`"))?;
            Ok((key, amount))
        })
        .collect()
}

/// Add `src` into `dst` per key, erroring on overflow rather than wrapping.
fn accumulate(dst: &mut BTreeMap<u32, u128>, src: &BTreeMap<u32, u128>) -> anyhow::Result<()> {
    for (k, v) in src {
        let slot = dst.entry(*k).or_insert(0);
        *slot =
            slot.checked_add(*v).ok_or_else(|| anyhow!("balance sum for token {k} overflows"))?;
    }
    Ok(())
}

fn boc_hash(ctx: &Arc<ClientContext>, boc: &str) -> anyhow::Result<String> {
    let out = get_boc_hash(Arc::clone(ctx), ParamsOfGetBocHash { boc: boc.to_string() })
        .map_err(|err| anyhow!("boc hash: {err}"))?;
    Ok(out.hash)
}

fn boc_depth(ctx: &Arc<ClientContext>, boc: &str) -> anyhow::Result<u32> {
    let out = get_boc_depth(Arc::clone(ctx), ParamsOfGetBocDepth { boc: boc.to_string() })
        .map_err(|err| anyhow!("boc depth: {err}"))?;
    Ok(out.depth)
}

/// The code cell of a TVC image, as a base64 BOC.
fn tvc_code_boc(ctx: &Arc<ClientContext>, tvc: &[u8]) -> anyhow::Result<String> {
    let out = get_code_from_tvc(
        Arc::clone(ctx),
        ParamsOfGetCodeFromTvc { tvc: BASE64_STANDARD.encode(tvc) },
    )
    .map_err(|err| anyhow!("get code from tvc: {err}"))?;
    Ok(out.code)
}

/// The salt PMP and OrderBook are deployed with: `abi.encode(PrivateNoteCode)`.
///
/// In the TVM ABI, `abi.encode(TvmCell)` stores the cell as a *reference*, so
/// the salt is a wrapper cell with no data bits and exactly one ref — not the
/// PrivateNote code cell itself. Salting with the raw code cell produces a
/// different hash, and one that looks entirely reasonable, so the check would
/// simply fail against a correct stand. `DexLib.buildPMPCode` /
/// `buildOrderBookCode` are the contracts' side of this.
fn salt_wrapper(ctx: &Arc<ClientContext>, code_boc: &str) -> anyhow::Result<String> {
    let out = encode_boc(
        Arc::clone(ctx),
        ParamsOfEncodeBoc {
            builder: vec![BuilderOp::CellBoc { boc: code_boc.to_string() }],
            boc_cache: None,
        },
    )
    .map_err(|err| anyhow!("encode salt wrapper: {err}"))?;
    Ok(out.boc)
}

/// Repr-hash of `base_code` once `salt` has been set on it.
fn salted_code_hash(
    ctx: &Arc<ClientContext>,
    base_code: &str,
    salt: &str,
) -> anyhow::Result<String> {
    let salted = set_code_salt(
        Arc::clone(ctx),
        ParamsOfSetCodeSalt {
            code: base_code.to_string(),
            salt: salt.to_string(),
            boc_cache: None,
        },
    )
    .map_err(|err| anyhow!("set code salt: {err}"))?;
    boc_hash(ctx, &salted.code)
}

/// Reads a shipped TVC and returns its code cell, having first confirmed the
/// file is the one the manifest pins.
///
/// Without that confirmation the salted comparison would prove nothing: a
/// swapped TVC on the host would derive a salted hash that matches the
/// contract deployed from it, and both would be wrong together. Resolution is
/// by `tvc_rel_path`, so a path the manifest does not ship is an error rather
/// than a silently unpinned input.
fn shipped_tvc_code(
    ctx: &Arc<ClientContext>,
    manifest: &Manifest,
    dir: &Path,
    rel_path: &str,
) -> anyhow::Result<String> {
    let entry = manifest
        .contracts
        .iter()
        .find(|c| c.tvc_rel_path.as_deref() == Some(rel_path))
        .ok_or_else(|| anyhow!("manifest ships no TVC at `{rel_path}`"))?;

    let path = dir.join(rel_path);
    let bytes = std::fs::read(&path).with_context(|| format!("read tvc {}", path.display()))?;
    let code = tvc_code_boc(ctx, &bytes)?;

    let got_hash = normalize_u256_hex(&boc_hash(ctx, &code)?)?;
    let want_hash = normalize_u256_hex(&entry.code_hash)?;
    anyhow::ensure!(
        got_hash == want_hash,
        "shipped {} code hash {got_hash} != manifest {want_hash}",
        entry.name
    );

    let got_depth = boc_depth(ctx, &code)?;
    anyhow::ensure!(
        got_depth == u32::from(entry.code_depth),
        "shipped {} code depth {got_depth} != manifest {}",
        entry.name,
        entry.code_depth
    );
    Ok(code)
}

/// Raw code-hash check for a contract deployed from unsalted code — the
/// baked roots, the seed notes, and the Oracle/OracleEventList a scenario
/// creates at runtime.
#[allow(dead_code)] // consumed by the live scenario tests
pub async fn assert_raw_code(
    r: &ChainReader,
    deployed_addr: &str,
    manifest_name: &str,
) -> anyhow::Result<()> {
    let manifest = load_manifest()?;
    assert_raw_code_with(&manifest, r, deployed_addr, manifest_name).await
}

async fn assert_raw_code_with(
    manifest: &Manifest,
    r: &ChainReader,
    deployed_addr: &str,
    manifest_name: &str,
) -> anyhow::Result<()> {
    let boc = r
        .account_boc(deployed_addr)
        .await?
        .ok_or_else(|| anyhow!("{manifest_name} at {deployed_addr} does not exist on chain"))?;
    let deployed = normalize_u256_hex(&deployed_code_hash(&boc)?)?;
    let want = normalize_u256_hex(&manifest.contract(manifest_name)?.code_hash)?;
    anyhow::ensure!(
        deployed == want,
        "{manifest_name} at {deployed_addr}: deployed code hash {deployed} != manifest {want}"
    );
    Ok(())
}

/// Salted code-hash check for the PMP / OrderBook a scenario deploys.
///
/// `base_tvc_path` is the contract's own image, `salt_tvc_path` the
/// PrivateNote image whose code becomes the salt; both are resolved relative
/// to the manifest's directory, where they ship. The salted hash is derived
/// here from those pinned bytes rather than carried in the manifest — the
/// provenance chain is the same either way, because a substituted image is
/// caught by the raw hash the manifest pins for it.
#[allow(dead_code)] // consumed by the live scenario tests
pub async fn assert_salted_code(
    r: &ChainReader,
    deployed_addr: &str,
    base_tvc_path: &str,
    salt_tvc_path: &str,
) -> anyhow::Result<()> {
    let manifest = load_manifest()?;
    let dir = manifest_dir()?;

    let base_code = shipped_tvc_code(&r.ctx, &manifest, &dir, base_tvc_path)?;
    let salt_code = shipped_tvc_code(&r.ctx, &manifest, &dir, salt_tvc_path)?;
    let wrapper = salt_wrapper(&r.ctx, &salt_code)?;
    let want = normalize_u256_hex(&salted_code_hash(&r.ctx, &base_code, &wrapper)?)?;

    let boc = r
        .account_boc(deployed_addr)
        .await?
        .ok_or_else(|| anyhow!("no account at {deployed_addr} to check salted code against"))?;
    let deployed = normalize_u256_hex(&deployed_code_hash(&boc)?)?;
    anyhow::ensure!(
        deployed == want,
        "{deployed_addr}: deployed code hash {deployed} != {base_tvc_path} salted with \
         {salt_tvc_path} ({want})"
    );
    Ok(())
}

/// Refuse to run unless the stand matches the manifest. See the module doc
/// for the order of checks; every failure names the invariant that broke.
#[allow(dead_code)] // consumed by the live scenario tests
pub async fn run_preflight(r: &ChainReader) -> anyhow::Result<()> {
    let manifest = load_manifest()?;

    check_provenance(&manifest)?;
    check_embedded_abis(&manifest)?;
    check_root_code_hashes(&manifest, r).await?;

    let notes = load_seed_notes(&seed_path()?)?;
    anyhow::ensure!(!notes.is_empty(), "seed file declares no notes — nothing to verify");

    let root_pn = RootPn::new(Arc::clone(&r.ctx), dex_contract_params(RootPn::DEFAULT_ADDRESS));
    let mut deployed_sum: BTreeMap<u32, u128> = BTreeMap::new();
    for note in &notes {
        assert_raw_code_with(&manifest, r, &note.address, "PrivateNote").await?;
        check_note_address(&root_pn, note).await?;

        let fields = r.storage_fields(&note.address, PRIVATE_NOTE_ABI).await?;
        check_note_keys(r, note, &fields)?;
        check_note_swept(note, &fields)?;
        check_note_embedded_codes(&manifest, r, note, &fields)?;
        accumulate(&mut deployed_sum, &uint_map(&fields, "_balance")?)?;
    }

    check_root_pn_booking(r, &deployed_sum).await
}

/// Provenance closure: the manifest has to belong to the checkout the host is
/// actually running. Same commit on both sides, or the manifest describes
/// some other build and every hash in it is beside the point.
///
/// The failure names what the manifest *is* — the sources and toolchain it
/// records — because the useful next question after "this is not your
/// manifest" is "then whose is it?", and the record is right here.
fn check_provenance(manifest: &Manifest) -> anyhow::Result<()> {
    let env_sha = std::env::var(DODEX_SHA_ENV)
        .map_err(|_| anyhow!("provenance: {DODEX_SHA_ENV} is not set"))?;
    anyhow::ensure!(
        env_sha == manifest.dodex_sha,
        "provenance: {DODEX_SHA_ENV}={env_sha} != manifest.dodex_sha={} (that manifest was built \
         from acki {} with sold {} / sold_old {} / tvm-cli {})",
        manifest.dodex_sha,
        manifest.acki_sha,
        manifest.sold_ver,
        manifest.sold_old_ver,
        manifest.tvm_cli_ver
    );
    Ok(())
}

/// Every ABI our binaries decode storage through must be the ABI that was
/// compiled into the deployed contracts. Code hashes alone do not cover this:
/// if a `.sol` changes, the staged TVC and the staged ABI both move together
/// and the code check passes, while our own decoding still runs on the stale
/// committed schema — a note would then sweep clean on fields nobody read.
fn check_embedded_abis(manifest: &Manifest) -> anyhow::Result<()> {
    for (name, abi_json) in EMBEDDED_ABIS {
        let got = abi_semantic_hash(abi_json)
            .with_context(|| format!("semantic hash of embedded {name} abi"))?;
        let want = &manifest.contract(name)?.abi_sem_hash;
        anyhow::ensure!(
            &got == want,
            "{name}: embedded abi semantic hash {got} != manifest {want}"
        );
    }
    Ok(())
}

/// Raw code hashes of the contracts the zerostate bakes in: the two roots
/// with a wrapper-declared address, and the two force-placed AI roots whose
/// addresses only the manifest knows.
async fn check_root_code_hashes(manifest: &Manifest, r: &ChainReader) -> anyhow::Result<()> {
    assert_raw_code_with(manifest, r, RootPn::DEFAULT_ADDRESS, "RootPN").await?;
    assert_raw_code_with(manifest, r, RootOracle::DEFAULT_ADDRESS, "RootOracle").await?;
    for (key, name) in VANITY_ROOTS {
        let addr = manifest
            .deployed_addresses
            .get(*key)
            .ok_or_else(|| anyhow!("manifest.deployed_addresses has no `{key}`"))?;
        assert_raw_code_with(manifest, r, addr, name).await?;
    }
    Ok(())
}

/// The note's address must be the one RootPN derives from its
/// deposit-identifier hash. Checked before the note is used for anything: a
/// seed row pointing at some other account would otherwise be inspected,
/// leased and traded against under the wrong identity.
async fn check_note_address(root_pn: &RootPn, note: &SeedNote) -> anyhow::Result<()> {
    let derived = root_pn
        .get_private_note_address(ParamsOfGetPrivateNoteAddress {
            deposit_identifier_hash: note.dih_dec.clone(),
        })
        .await
        .map_err(|err| anyhow!("getPrivateNoteAddress({}): {err}", note.dih_dec))?
        .private_note_address;
    anyhow::ensure!(
        derived.eq_ignore_ascii_case(&note.address),
        "seed note {}: RootPN derives {derived} from dih {}",
        note.address,
        note.dih_dec
    );
    Ok(())
}

/// The keyring closure: the public key *derived from the secret* must equal
/// both the seed file's own public key and the key the note signs with.
/// Deriving is the point — comparing the file's two hex strings to each other
/// would pass with someone else's secret key, and the run would then fail at
/// the first signature with no visible connection to the cause.
fn check_note_keys(r: &ChainReader, note: &SeedNote, fields: &Value) -> anyhow::Result<()> {
    // The seed file's ground truth is the 32-byte ed25519 seed (64 hex);
    // `nacl_sign_keypair_from_secret_key` expects exactly that and rejects
    // the 128-hex secret-plus-public concatenation NaCl also calls a "secret".
    let derived = nacl_sign_keypair_from_secret_key(
        Arc::clone(&r.ctx),
        ParamsOfNaclSignKeyPairFromSecret { secret: note.keys.secret.clone() },
    )
    .map_err(|err| anyhow!("derive pubkey for {}: {err}", note.address))?;

    let derived = normalize_u256_hex(&derived.public)?;
    let declared = normalize_u256_hex(&note.keys.public)?;
    anyhow::ensure!(
        derived == declared,
        "seed note {}: pubkey derived from the secret ({derived}) != declared ({declared})",
        note.address
    );

    let onchain = owner_pubkey_from_value(fields)
        .with_context(|| format!("read _ephemeralPubkey of {}", note.address))?;
    anyhow::ensure!(
        derived == onchain,
        "seed note {}: seed keypair ({derived}) != on-chain _ephemeralPubkey ({onchain})",
        note.address
    );
    Ok(())
}

/// Notes must start the run clean, judged by the same sweep a note returning
/// to the pool has to pass.
fn check_note_swept(note: &SeedNote, fields: &Value) -> anyhow::Result<()> {
    match sweep_verdict(fields) {
        SweepVerdict::Clean => Ok(()),
        SweepVerdict::Dirty { fields } => {
            Err(anyhow!("seed note {} does not start clean: {fields:?}", note.address))
        }
    }
}

/// The child contract codes baked into a note's storage must be the ones the
/// manifest pins, in both of the two forms the ABI declares them.
fn check_note_embedded_codes(
    manifest: &Manifest,
    r: &ChainReader,
    note: &SeedNote,
    fields: &Value,
) -> anyhow::Result<()> {
    for (field, name) in NOTE_CODE_CELL_FIELDS {
        let boc = fields
            .get(field)
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("note {}: `{field}` is missing or not a cell", note.address))?;
        let got = normalize_u256_hex(&boc_hash(&r.ctx, boc)?)?;
        let want = normalize_u256_hex(&manifest.contract(name)?.code_hash)?;
        anyhow::ensure!(
            got == want,
            "note {}: embedded {name} code hash {got} != manifest {want}",
            note.address
        );
    }

    for (hash_field, depth_field, name) in NOTE_CODE_HASH_DEPTH_FIELDS {
        let entry = manifest.contract(name)?;

        let raw_hash = fields
            .get(hash_field)
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("note {}: `{hash_field}` is missing", note.address))?;
        let got = normalize_u256_hex(raw_hash)?;
        let want = normalize_u256_hex(&entry.code_hash)?;
        anyhow::ensure!(
            got == want,
            "note {}: embedded {name} code hash {got} != manifest {want}",
            note.address
        );

        // The depth is not decoration: child addresses are derived from the
        // (hash, depth) pair, so a right hash under a wrong depth still
        // resolves to the wrong accounts.
        let raw_depth = fields
            .get(depth_field)
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("note {}: `{depth_field}` is missing", note.address))?;
        let got_depth: u16 = raw_depth
            .parse()
            .with_context(|| format!("note {}: `{depth_field}` = `{raw_depth}`", note.address))?;
        anyhow::ensure!(
            got_depth == entry.code_depth,
            "note {}: embedded {name} code depth {got_depth} != manifest {}",
            note.address,
            entry.code_depth
        );
    }
    Ok(())
}

/// RootPN's booking must match the notes it baked, and its physical holdings
/// must cover that booking.
///
/// The booking half matters because `RootPN.withdrawTokens` requires
/// `_deployedValues[tt] >= amount`: a zerostate that skipped it sends every
/// withdrawal of a baked note into the revert path, and a scenario that never
/// withdraws would pass on a stand no other wave can use.
///
/// Token types and extra-currency ids share one numbering (NACKL 1, SHELL 2,
/// USDC 3 — see `proof::TokenType`), which is what makes the physical
/// comparison meaningful per token type.
async fn check_root_pn_booking(
    r: &ChainReader,
    note_sum: &BTreeMap<u32, u128>,
) -> anyhow::Result<()> {
    let fields = r.storage_fields(RootPn::DEFAULT_ADDRESS, ROOT_PN_ABI).await?;
    let booked = uint_map(&fields, "_deployedValues")?;
    let physical = r.account_ecc(RootPn::DEFAULT_ADDRESS).await?;

    // Union of both key sets: a token type booked but not held by any note is
    // just as much a mismatch as the reverse.
    let token_types: std::collections::BTreeSet<u32> =
        booked.keys().chain(note_sum.keys()).copied().collect();
    for tt in token_types {
        let want = note_sum.get(&tt).copied().unwrap_or(0);
        let got = booked.get(&tt).copied().unwrap_or(0);
        anyhow::ensure!(
            got == want,
            "RootPN._deployedValues[{tt}] = {got}, but the seed notes hold {want}"
        );
        let held = physical.ecc.get(&tt).copied().unwrap_or(0);
        anyhow::ensure!(
            held >= want,
            "RootPN physically holds {held} of currency {tt}, less than the {want} it booked"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use dodex_infrastructure::tvm_runner::boc_cell_shape;
    use serde_json::json;

    use super::*;

    /// A client context with no network configured. Every BOC operation these
    /// tests drive is local, so nothing here reaches a network; a real run
    /// passes `ChainReader`'s own context instead.
    fn offline_context() -> Arc<ClientContext> {
        Arc::new(ClientContext::new(ClientConfig::default()).expect("create offline tvm context"))
    }

    /// A committed `PrivateNote`/`PMP` image, used only as a source of two
    /// real, salt-capable code cells. Nothing here depends on these being the
    /// bytes any particular stand runs — the properties under test are
    /// structural.
    const PN_TVC: &[u8] = include_bytes!("../../../../contracts/dex/PrivateNote.tvc");
    const PMP_TVC: &[u8] = include_bytes!("../../../../contracts/dex/PMP.tvc");

    #[test]
    fn abi_semantic_hash_is_key_order_invariant() {
        let a = r#"{"b":1,"a":{"y":2,"x":[3,4]}}"#;
        let b = r#"{ "a": {"x":[3,4], "y":2}, "b": 1 }"#;
        assert_eq!(abi_semantic_hash(a).unwrap(), abi_semantic_hash(b).unwrap());
    }

    #[test]
    fn canonical_json_matches_python_dumps_byte_for_byte() {
        // Pinned against the exact output of
        //   json.dumps(json.loads(SRC), sort_keys=True, separators=(",",":"))
        // for the same input. Covers, in one shot, every way the two
        // implementations could drift: key order (empty / upper / lower /
        // non-ASCII), separator spacing, short escapes, `\uXXXX` escapes for
        // control characters, and the surrogate pair Python emits for a
        // non-BMP character.
        let src = r#"{"b": 1, "a": {"y": 2, "x": [3, 4]}, "\u00e9": "caf\u00e9 \u0001 \t \" \\ \ud83d\ude00", "": null, "A": false}"#;
        let want = r#"{"":null,"A":false,"a":{"x":[3,4],"y":2},"b":1,"\u00e9":"caf\u00e9 \u0001 \t \" \\ \ud83d\ude00"}"#;

        let value: Value = serde_json::from_str(src).unwrap();
        let mut got = String::new();
        canonical_json(&value, &mut got).unwrap();

        assert_eq!(got, want);
    }

    #[test]
    fn abi_semantic_hash_pins_cross_language_reference() {
        // The cross-language constant: `gen_manifest.py --selftest` prints
        // this same hex for the same input. If the two ever disagree, every
        // ABI comparison preflight makes is meaningless, so it is pinned
        // here as a literal rather than recomputed.
        assert_eq!(
            abi_semantic_hash(r#"{"a":1}"#).unwrap(),
            "015abd7f5cc57a2dd94b7590f04ad8084273905ee33ec5cebeae62276a97f862"
        );
    }

    #[test]
    fn abi_semantic_hash_rejects_a_float() {
        // Python's float repr and serde_json's differ, so a float would
        // silently produce two different digests. There are no floats in an
        // ABI; one appearing means the input is not an ABI.
        assert!(abi_semantic_hash(r#"{"a":1.5}"#).is_err());
    }

    #[test]
    fn normalize_u256_accepts_prefixed_and_unprefixed() {
        let h = "AB".repeat(32);
        assert_eq!(normalize_u256_hex(&format!("0x{h}")).unwrap(), "ab".repeat(32));
        assert_eq!(normalize_u256_hex(&h).unwrap(), "ab".repeat(32));
        assert!(normalize_u256_hex("0xdead").is_err()); // not 64 hex — an error, not padding
    }

    #[test]
    fn normalize_u256_hex_rejects_non_hex_digits() {
        // Right length, wrong alphabet: must not be accepted just because it
        // measures 64 characters.
        assert!(normalize_u256_hex(&"z".repeat(64)).is_err());
    }

    #[test]
    fn manifest_parses_the_generator_shape() {
        let m: Manifest = serde_json::from_str(MANIFEST_FIXTURE).unwrap();

        assert_eq!(m.dodex_sha, "deadbeef");
        assert_eq!(m.images.get("node").map(String::as_str), Some("node@sha256:aa"));
        assert_eq!(m.deployed_addresses.get("super_root").map(String::as_str), Some(SUPER_ROOT));

        let pmp = m.contract("PMP").unwrap();
        assert_eq!(pmp.code_depth, 7);
        assert_eq!(pmp.tvc_rel_path.as_deref(), Some("PMP.tvc"));
        assert!(m.contract("Nullifier").unwrap().tvc_rel_path.is_none());
        assert!(m.contract("NotThere").is_err());
    }

    const SUPER_ROOT: &str = "0:0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c";

    /// Shaped exactly like `gen_manifest.py`'s output, down to `null` for a
    /// contract that ships no TVC.
    const MANIFEST_FIXTURE: &str = r#"{
      "acki_sha": "cafe",
      "contracts": [
        {"name": "PMP", "code_hash": "aa", "code_depth": 7, "abi_sem_hash": "bb", "tvc_rel_path": "PMP.tvc"},
        {"name": "Nullifier", "code_hash": "cc", "code_depth": 3, "abi_sem_hash": "dd", "tvc_rel_path": null}
      ],
      "deployed_addresses": {
        "model_registry": "0:0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d",
        "super_root": "0:0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c"
      },
      "dodex_sha": "deadbeef",
      "images": {"allbin": "allbin@sha256:dd", "bm": "bm@sha256:cc", "gql": "gql@sha256:bb", "node": "node@sha256:aa"},
      "sold_old_ver": "0.76.0",
      "sold_ver": "0.80.0",
      "tvm_cli_ver": "0.42.0"
    }"#;

    #[test]
    fn embedded_abis_cover_every_abi_compiled_into_the_contract_wrappers() {
        // The check is only worth what its list covers: an ABI that
        // `crates/contracts` compiles in but this list forgets is an ABI
        // preflight never compares, and a drifted schema there decodes
        // silently wrong storage instead of failing the run. Closed against
        // the wrapper crate's actual `include_str!` set so adding a wrapper
        // fails here until the ABI is listed.
        let listed: BTreeSet<&str> = EMBEDDED_ABIS.iter().map(|(name, _)| *name).collect();
        let compiled_in = abi_names_included_by_contracts_crate();
        assert_eq!(listed, compiled_in);
    }

    /// Every `<Name>.abi.json` reached by an `include_str!` anywhere under
    /// `crates/contracts/src`.
    fn abi_names_included_by_contracts_crate() -> BTreeSet<&'static str> {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../crates/contracts/src");
        let mut found = BTreeSet::new();
        let mut stack = vec![PathBuf::from(root)];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read crates/contracts/src") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                let src = std::fs::read_to_string(&path).expect("read wrapper source");
                for chunk in src.split("include_str!(\"").skip(1) {
                    let Some(literal) = chunk.split('"').next() else { continue };
                    let Some(file) = literal.strip_suffix(".abi.json") else { continue };
                    let name = file.rsplit('/').next().expect("non-empty path");
                    // Leaked so the set can borrow `&'static str` and compare
                    // directly against `EMBEDDED_ABIS`' literals.
                    found.insert(&*Box::leak(name.to_string().into_boxed_str()));
                }
            }
        }
        found
    }

    #[test]
    fn salt_wrapper_is_a_bare_reference_to_the_code_cell() {
        // `abi.encode(TvmCell)` puts the cell in as a REFERENCE: the salt is
        // a cell with no data bits and exactly one ref. Asserting the shape,
        // not just the hash, is what makes this test fail if the wrapper is
        // ever "simplified" into the raw code cell.
        let ctx = offline_context();
        let pn_code = tvc_code_boc(&ctx, PN_TVC).unwrap();

        let wrapper = salt_wrapper(&ctx, &pn_code).unwrap();

        let shape = boc_cell_shape(&wrapper).unwrap();
        assert_eq!(shape.data_bits, 0);
        assert_eq!(shape.refs, vec![boc_hash(&ctx, &pn_code).unwrap()]);
    }

    #[test]
    fn salting_with_the_wrapper_differs_from_salting_with_the_raw_code() {
        // The regression this exists for: salting with the raw PrivateNote
        // code cell instead of the wrapper yields a different, wrong hash —
        // and one that looks entirely plausible.
        let ctx = offline_context();
        let pn_code = tvc_code_boc(&ctx, PN_TVC).unwrap();
        let pmp_code = tvc_code_boc(&ctx, PMP_TVC).unwrap();
        let wrapper = salt_wrapper(&ctx, &pn_code).unwrap();

        let with_wrapper = salted_code_hash(&ctx, &pmp_code, &wrapper).unwrap();
        let with_raw_code = salted_code_hash(&ctx, &pmp_code, &pn_code).unwrap();
        let unsalted = boc_hash(&ctx, &pmp_code).unwrap();

        assert_ne!(with_wrapper, with_raw_code);
        assert_ne!(with_wrapper, unsalted);
        assert_ne!(with_raw_code, unsalted);
    }

    #[test]
    fn uint_map_rejects_a_malformed_entry() {
        let good = json!({"_deployedValues": {"1": "5", "2": "7"}});
        let got = uint_map(&good, "_deployedValues").unwrap();
        assert_eq!(got.get(&1), Some(&5u128));
        assert_eq!(got.get(&2), Some(&7u128));

        // A value the decoder could not have produced must be an error, not
        // a silently dropped entry: a dropped entry makes a conservation sum
        // too small, which is a wrong answer rather than a loud one.
        assert!(uint_map(&json!({"_deployedValues": {"1": "oops"}}), "_deployedValues").is_err());
        // A missing field means the ABI no longer matches the deployed
        // contract — fail-closed, exactly like the sweep lists.
        assert!(uint_map(&json!({}), "_deployedValues").is_err());
    }
}
