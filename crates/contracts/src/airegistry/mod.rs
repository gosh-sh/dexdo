//! AI Registry contract wrappers and typed event decoders.
//!
//! The modules in this namespace mirror the Solidity contracts under
//! `contracts/airegistry/` and follow the same wrapper style as `dex`
//! (`ContractBase + HasContractBase + AutoContract`, traits from
//! `ackinacki_kit`). Event ids come from
//! `contracts/airegistry/modifiers/modifiers.sol`.
//!
//! The registry stack is `SuperRoot → RootModel → TokenContract`; an
//! `InferenceOrderBook` is deployed per `(model, tick)` from a
//! `PrivateNote` (the inference proxy methods live on `dex::private_note`,
//! since the note is the on-chain participant that holds the SHELL escrow).

pub mod inference_order_book;
pub mod inference_order_book_events;
pub mod root_model;
pub mod root_model_events;
pub mod super_root;
pub mod super_root_events;
pub mod token_contract;
pub mod token_contract_events;

#[cfg(test)]
mod tests;
