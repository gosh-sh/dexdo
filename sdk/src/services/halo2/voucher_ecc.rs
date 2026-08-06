//! The ECC currency map a `RootPN.generateVoucher` message must carry.
//!
//! `generateVoucher` accepts four shapes and refuses everything else. The rule
//! lives in `contracts/dex/RootPN.sol`; this is the client side of it, in one
//! place, because the giver flow and the multisig flow both build this map and
//! two copies of one requirement drift apart.
//!
//! ```text
//!   one currency,  isFee=true   -> take nothing; the whole amount is the nominal.
//!                                  The currency MUST be SHELL.
//!   one currency,  isFee=false  -> take GAS_DEPOSIT; nominal is the remainder.
//!                                  The currency MUST be SHELL.
//!   two currencies              -> the SHELL leg is the gas, EXACTLY GAS_DEPOSIT,
//!                                  consumed whole; the other currency is the
//!                                  nominal in full, whatever isFee says.
//!   anything else               -> refused (ERR_BAD_GAS_MIX).
//! ```
//!
//! ## Why a NACKL deposit needs two legs
//!
//! `require(tokenType == CURRENCIES_ID_SHELL, ERR_BAD_GAS_MIX)` guards the
//! single-currency branch — both halves of it. An earlier contract only guarded
//! the non-fee half, and the gap was a mint: a NACKL deposit carrying `isFee`
//! produced an ordinary NACKL voucher with no gas taken, that voucher deployed a
//! note, and the root handed that note `GAS_DEPOSIT` out of the common pool, with
//! nothing bounding the loop.
//!
//! Closing it means a non-SHELL nominal can no longer arrive alone. It must bring
//! its own gas as a second leg — which is what this module adds. A client that
//! keeps sending one leg does not get an error it can see: `generateVoucher`
//! reverts before `emit VoucherGenerated`, so the caller waits out its event
//! timeout against a chain that is busy emitting other people's vouchers.
//!
//! ## The SHELL leg is an equality, not a minimum
//!
//! `require(uint128(msg.currencies[shellLeg]) == GAS_DEPOSIT)`. Sending more is
//! refused exactly like sending less, so this figure is not a budget to pad.

use std::collections::HashMap;

/// `CURRENCIES_ID_SHELL` in `contracts/dex/modifiers/modifiers.sol`.
pub const CURRENCIES_ID_SHELL: u32 = 2;

/// `GAS_DEPOSIT` in `contracts/dex/modifiers/modifiers.sol` — 250 SHELL, the
/// gas every non-gas deposit pays so the note it deploys can run at all. A note
/// is created cross-dapp, where plain native value does not reach; only ECC
/// crosses and converts.
pub const GAS_DEPOSIT: u64 = 250_000_000_000;

/// The ECC map for a `generateVoucher` carrying `voucher_value` of
/// `voucher_token_type`.
///
/// A non-SHELL nominal gets the mandatory SHELL gas leg beside it. A SHELL
/// nominal is already the one currency the contract accepts alone, and cannot
/// take a second leg of the same currency, so it is returned as-is.
///
/// ## What the contract will emit as the nominal
///
/// Two legs: the full `voucher_value`. One leg with `isFee`: the full
/// `voucher_value`. One leg without `isFee`: `voucher_value - GAS_DEPOSIT`, the
/// only shape where the two differ — and `RootPN.deployPrivateNote` binds the
/// `value` a caller passes to the nominal committed in the proof
/// (`require(_u64ToFr(value) == voucherNominalFr)`), so a caller taking that
/// path must send `voucher_value` and then claim the reduced figure. No caller
/// does: every deposit in this repo is non-SHELL, and every SHELL voucher is a
/// gas voucher with `isFee`.
pub fn generate_voucher_ecc(voucher_token_type: u32, voucher_value: u64) -> HashMap<u32, u64> {
    let mut ecc = HashMap::new();
    ecc.insert(voucher_token_type, voucher_value);
    if voucher_token_type != CURRENCIES_ID_SHELL {
        ecc.insert(CURRENCIES_ID_SHELL, GAS_DEPOSIT);
    }
    ecc
}

#[cfg(test)]
mod tests {
    use super::*;

    const NACKL: u32 = 1;
    const USDC: u32 = 3;

    #[test]
    fn a_non_shell_nominal_brings_its_own_gas_leg() {
        let ecc = generate_voucher_ecc(NACKL, 10_000_000_000_000);
        assert_eq!(ecc.len(), 2, "a NACKL deposit alone is ERR_BAD_GAS_MIX: {ecc:?}");
        assert_eq!(ecc.get(&NACKL), Some(&10_000_000_000_000), "the nominal arrives in full");
        assert_eq!(
            ecc.get(&CURRENCIES_ID_SHELL),
            Some(&GAS_DEPOSIT),
            "the SHELL leg is an equality in the contract, so it is neither padded nor trimmed"
        );
    }

    #[test]
    fn every_non_shell_currency_gets_the_leg_not_just_nackl() {
        // The rule is "not SHELL", not "is NACKL" — USDC deposits take the same
        // path and would fail the same way.
        let ecc = generate_voucher_ecc(USDC, 5_000_000);
        assert_eq!(ecc.get(&USDC), Some(&5_000_000));
        assert_eq!(ecc.get(&CURRENCIES_ID_SHELL), Some(&GAS_DEPOSIT));
    }

    #[test]
    fn a_shell_nominal_stays_a_single_leg() {
        // Two legs of one currency cannot exist in the map, and SHELL alone is
        // a shape the contract accepts. This is the gas-voucher path.
        let ecc = generate_voucher_ecc(CURRENCIES_ID_SHELL, 300_000_000_000);
        assert_eq!(ecc.len(), 1, "a second SHELL leg is not expressible: {ecc:?}");
        assert_eq!(ecc.get(&CURRENCIES_ID_SHELL), Some(&300_000_000_000));
    }

    #[test]
    fn the_gas_leg_never_overwrites_the_nominal() {
        // Guards the ordering inside the builder: inserting the gas leg after
        // the nominal must not clobber it when a future caller passes a SHELL
        // nominal through the non-SHELL branch by mistake.
        let ecc = generate_voucher_ecc(CURRENCIES_ID_SHELL, GAS_DEPOSIT + 1);
        assert_eq!(ecc.get(&CURRENCIES_ID_SHELL), Some(&(GAS_DEPOSIT + 1)));
    }

    /// The decimal value of `<ty> <name> = …;` in `src`.
    ///
    /// The name is matched whole. `CURRENCIES_ID_SHELL` is a prefix of
    /// `CURRENCIES_ID_SHELL_FEE`, and a prefix match would read 300 as the
    /// SHELL id the day someone reorders the block.
    fn solidity_constant(src: &str, ty: &str, name: &str) -> Option<u128> {
        src.lines().find_map(|line| {
            let rest = line.trim().strip_prefix(ty)?.trim_start().strip_prefix(name)?;
            let value = rest.trim_start().strip_prefix('=')?.split(';').next()?;
            value.trim().replace('_', "").parse().ok()
        })
    }

    #[test]
    fn the_constants_match_the_contract() {
        // Read out of the contract rather than restated, so a re-pin there
        // fails here instead of on chain 480 seconds into a CI run.
        let modifiers = include_str!("../../../../contracts/dex/modifiers/modifiers.sol");

        assert_eq!(
            Some(u128::from(GAS_DEPOSIT)),
            solidity_constant(modifiers, "uint128 constant", "GAS_DEPOSIT"),
            "GAS_DEPOSIT drifted from the contract — the SHELL leg is an exact equality there, so \
             a stale figure is refused outright"
        );
        assert_eq!(
            Some(u128::from(CURRENCIES_ID_SHELL)),
            solidity_constant(modifiers, "uint32 constant", "CURRENCIES_ID_SHELL"),
            "CURRENCIES_ID_SHELL drifted from the contract"
        );
    }

    #[test]
    fn the_constant_reader_does_not_confuse_a_prefix_for_the_name() {
        // The guard above is only worth having if it distinguishes these two,
        // so that is asserted rather than assumed.
        let src = "    uint32 constant CURRENCIES_ID_SHELL_FEE = 300;\n    \
                   uint32 constant CURRENCIES_ID_SHELL = 2;\n";
        assert_eq!(solidity_constant(src, "uint32 constant", "CURRENCIES_ID_SHELL"), Some(2));
        assert_eq!(solidity_constant(src, "uint32 constant", "CURRENCIES_ID_SHELL_FEE"), Some(300));
    }
}
