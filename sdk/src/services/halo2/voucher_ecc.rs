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

use crate::services::halo2::paths::Halo2PathsError;

/// `CURRENCIES_ID_SHELL` in `contracts/dex/modifiers/modifiers.sol`.
pub const CURRENCIES_ID_SHELL: u32 = 2;

/// `GAS_DEPOSIT` in `contracts/dex/modifiers/modifiers.sol` — 250 SHELL, the
/// gas every non-gas deposit pays so the note it deploys can run at all. A note
/// is created cross-dapp, where plain native value does not reach; only ECC
/// crosses and converts.
pub const GAS_DEPOSIT: u64 = 250_000_000_000;

/// What to send for a voucher, and what the contract will emit for it.
#[derive(Debug)]
pub struct VoucherSend {
    /// The ECC map to attach to the `generateVoucher` message.
    pub ecc: HashMap<u32, u64>,
    /// The nominal `VoucherGenerated` will carry.
    ///
    /// This — not the sum sent — is what a caller passes onward as `value`:
    /// `RootPN.deployPrivateNote` binds the two with
    /// `require(_u64ToFr(value) == voucherNominalFr)`, and the proof commits to
    /// the figure in the event.
    pub nominal: u64,
}

/// Plan a `generateVoucher` for a note that should end up holding `nominal` of
/// `voucher_token_type`.
///
/// `nominal` is what the NOTE receives, not what the wallet pays. The gas is
/// charged on top in every deposit shape, so the wallet always spends
/// `nominal + GAS_DEPOSIT` on a deposit and `nominal` on a gas voucher —
/// regardless of currency. Which of the contract's shapes carries that is a
/// detail of how the two amounts can be expressed in one currency map:
///
/// - a non-SHELL nominal cannot share a map key with its gas, so the gas rides
///   as a second leg and the contract deducts nothing;
/// - a SHELL nominal has nowhere else to put the gas — one currency, one key —
///   so it is added to the single leg and the contract subtracts it back out;
/// - a gas voucher (`is_fee`) IS the gas, and charging gas for buying gas would
///   be circular, so nothing is added and nothing is deducted.
///
/// The `nominal` returned is equal to the argument in all three. That equality
/// is the point: it is what lets a caller pass one figure to
/// `deployPrivateNote` without knowing which shape was used.
pub fn plan_voucher(
    voucher_token_type: u32,
    nominal: u64,
    is_fee: bool,
) -> Result<VoucherSend, Halo2PathsError> {
    let mut ecc = HashMap::new();
    if voucher_token_type != CURRENCIES_ID_SHELL {
        ecc.insert(voucher_token_type, nominal);
        ecc.insert(CURRENCIES_ID_SHELL, GAS_DEPOSIT);
    } else if is_fee {
        ecc.insert(CURRENCIES_ID_SHELL, nominal);
    } else {
        // `require(voucherNominal >= GAS_DEPOSIT)` then `voucherNominal -=
        // GAS_DEPOSIT`. Sending the bare nominal would deploy a note short by
        // 250 SHELL and, because the reduced figure is not an allowed nominal,
        // would be refused before it got that far.
        //
        // Checked, not saturating: a clamped sum is neither the nominal nor the
        // nominal plus gas, and the contract would subtract the gas from it and
        // deploy against whatever was left.
        let with_gas = nominal
            .checked_add(GAS_DEPOSIT)
            .ok_or(Halo2PathsError::VoucherGasOverflow { nominal, gas: GAS_DEPOSIT })?;
        ecc.insert(CURRENCIES_ID_SHELL, with_gas);
    }
    Ok(VoucherSend { ecc, nominal })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NACKL: u32 = 1;
    const USDC: u32 = 3;

    const N10000: u64 = 10_000_000_000_000;

    #[test]
    fn a_non_shell_nominal_brings_its_own_gas_leg() {
        let plan = plan_voucher(NACKL, N10000, false).unwrap();
        assert_eq!(plan.ecc.len(), 2, "a NACKL deposit alone is ERR_BAD_GAS_MIX: {:?}", plan.ecc);
        assert_eq!(plan.ecc.get(&NACKL), Some(&N10000), "the nominal arrives in full");
        assert_eq!(
            plan.ecc.get(&CURRENCIES_ID_SHELL),
            Some(&GAS_DEPOSIT),
            "the SHELL leg is an equality in the contract, so it is neither padded nor trimmed"
        );
        assert_eq!(plan.nominal, N10000, "two legs: the contract deducts nothing");
    }

    #[test]
    fn every_non_shell_currency_gets_the_leg_not_just_nackl() {
        // The rule is "not SHELL", not "is NACKL" — USDC deposits take the same
        // path and would fail the same way.
        let plan = plan_voucher(USDC, 5_000_000, false).unwrap();
        assert_eq!(plan.ecc.get(&USDC), Some(&5_000_000));
        assert_eq!(plan.ecc.get(&CURRENCIES_ID_SHELL), Some(&GAS_DEPOSIT));
        assert_eq!(plan.nominal, 5_000_000);
    }

    #[test]
    fn a_shell_deposit_pays_its_gas_inside_the_single_leg() {
        // One currency, one map key: the gas cannot ride beside it, so it is
        // added on and the contract takes it back out. Sending the bare nominal
        // is the bug this shape exists to prevent — the note would come up 250
        // SHELL short, and the reduced figure is not an allowed nominal.
        let plan = plan_voucher(CURRENCIES_ID_SHELL, N10000, false).unwrap();
        assert_eq!(plan.ecc.len(), 1, "a second SHELL leg is not expressible: {:?}", plan.ecc);
        assert_eq!(plan.ecc.get(&CURRENCIES_ID_SHELL), Some(&(N10000 + GAS_DEPOSIT)));
        assert_eq!(plan.nominal, N10000, "what the note receives, after the contract's deduction");
    }

    #[test]
    fn a_gas_voucher_pays_no_gas() {
        // Charging gas for buying gas would be circular, so `isFee` neither adds
        // nor deducts. This is the path every SHELL voucher in the repo takes.
        let plan = plan_voucher(CURRENCIES_ID_SHELL, 100_000_000_000, true).unwrap();
        assert_eq!(plan.ecc.get(&CURRENCIES_ID_SHELL), Some(&100_000_000_000));
        assert_eq!(plan.nominal, 100_000_000_000);
    }

    #[test]
    fn a_nominal_that_cannot_carry_its_gas_is_refused_rather_than_clamped() {
        // Only the single-leg SHELL deposit adds the two together, so it is the
        // only shape that can overflow. Saturating would be the worst outcome:
        // a sum that is neither the nominal nor the nominal plus gas, which the
        // contract would then subtract the gas from and deploy against.
        let err = plan_voucher(CURRENCIES_ID_SHELL, u64::MAX, false);
        assert!(matches!(err, Err(Halo2PathsError::VoucherGasOverflow { .. })), "{err:?}");

        // The largest nominal that still fits must go through untouched — an
        // off-by-one here would refuse a legitimate deposit.
        let edge = plan_voucher(CURRENCIES_ID_SHELL, u64::MAX - GAS_DEPOSIT, false).unwrap();
        assert_eq!(edge.ecc.get(&CURRENCIES_ID_SHELL), Some(&u64::MAX));
        assert_eq!(edge.nominal, u64::MAX - GAS_DEPOSIT);
    }

    #[test]
    fn the_other_shapes_cannot_overflow_at_all() {
        // They never add, so `u64::MAX` is an ordinary nominal for them.
        assert!(plan_voucher(NACKL, u64::MAX, false).is_ok());
        assert!(plan_voucher(CURRENCIES_ID_SHELL, u64::MAX, true).is_ok());
    }

    #[test]
    fn the_note_receives_the_asked_for_nominal_in_every_shape() {
        // The invariant callers depend on: whatever the shape, the figure handed
        // to `deployPrivateNote` as `value` is the one that was asked for, and
        // `require(_u64ToFr(value) == voucherNominalFr)` holds.
        for (token, is_fee) in [
            (NACKL, false),
            (USDC, false),
            (CURRENCIES_ID_SHELL, false),
            (CURRENCIES_ID_SHELL, true),
        ] {
            assert_eq!(
                plan_voucher(token, N10000, is_fee).unwrap().nominal,
                N10000,
                "token={token} is_fee={is_fee}"
            );
        }
    }

    #[test]
    fn a_deposit_always_costs_the_nominal_plus_the_gas() {
        // Same total out of the wallet either way — the currency only decides
        // which leg carries it. A wallet budgeted for one shape is budgeted for
        // the other.
        let nackl: u64 = plan_voucher(NACKL, N10000, false).unwrap().ecc.values().sum();
        let shell: u64 =
            plan_voucher(CURRENCIES_ID_SHELL, N10000, false).unwrap().ecc.values().sum();
        assert_eq!(nackl, N10000 + GAS_DEPOSIT);
        assert_eq!(shell, N10000 + GAS_DEPOSIT);
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
