use std::collections::hash_map::Entry;
use std::collections::HashMap;

use ackinacki_kit::contracts::dex::private_note::ResultOfGetDetails as KitPnDetails;
use ackinacki_kit::contracts::dex::private_note::ResultOfGetStakes as KitPnStakes;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivateNoteDetails {
    pub deposit_identifier_hash: String,
    pub ephemeral_pubkey: String,
    pub balance: HashMap<String, u128>,
    pub pmp_code_hash: String,
    pub private_note_code_hash: String,
    pub busy_address: Option<String>,
    pub coupons_value: u128,
    pub has_withdrawn: bool,
}

impl From<KitPnDetails> for PrivateNoteDetails {
    fn from(d: KitPnDetails) -> Self {
        Self {
            deposit_identifier_hash: d.deposit_identifier_hash,
            ephemeral_pubkey: d.ephemeral_pubkey,
            balance: d.balance,
            pmp_code_hash: d.pmp_code_hash,
            private_note_code_hash: d.private_note_code_hash,
            busy_address: d.busy_address,
            coupons_value: d.coupons_value,
            has_withdrawn: d.has_withdrawn,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivateNoteStakes {
    pub stakes: HashMap<String, serde_json::Value>,
}

impl From<KitPnStakes> for PrivateNoteStakes {
    fn from(s: KitPnStakes) -> Self {
        Self { stakes: s.stakes }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultOfBlockchainWrite {
    pub message_hash: Option<String>,
    pub block_hash: Option<String>,
    pub tx_hash: Option<String>,
}

impl From<ackinacki_kit::tvm_client::processing::ResultOfSendMessage> for ResultOfBlockchainWrite {
    fn from(r: ackinacki_kit::tvm_client::processing::ResultOfSendMessage) -> Self {
        Self { message_hash: r.message_hash, block_hash: r.block_hash, tx_hash: r.tx_hash }
    }
}

/// A PrivateNote discovered by scanning deploy events and matching
/// ephemeral_pubkey.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredNote {
    pub deposit_identifier_hash: String,
    pub note_address: String,
    pub initial_balance: u128,
    pub ephemeral_pubkey: String,
    pub token_type: Option<u32>,
}

impl From<crate::services::discovery::DiscoveredNote> for DiscoveredNote {
    fn from(d: crate::services::discovery::DiscoveredNote) -> Self {
        Self {
            deposit_identifier_hash: d.deposit_identifier_hash,
            note_address: d.note_address,
            initial_balance: d.initial_balance,
            ephemeral_pubkey: d.ephemeral_pubkey,
            token_type: d.token_type,
        }
    }
}

/// Aggregated balance across multiple PrivateNotes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregatedBalance {
    /// Total balance per token_type (key = token_type string, value = sum
    /// across all PNs).
    pub totals: HashMap<String, u128>,
    /// Per-PN breakdown.
    pub notes: Vec<PrivateNoteBalance>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivateNoteBalance {
    pub address: String,
    pub balance: HashMap<String, u128>,
}

impl AggregatedBalance {
    pub fn from_details(notes: Vec<(String, PrivateNoteDetails)>) -> Self {
        let mut totals: HashMap<String, u128> = HashMap::new();
        let mut pn_balances = Vec::with_capacity(notes.len());

        for (address, details) in notes {
            for (token_type, amount) in &details.balance {
                match totals.entry(token_type.clone()) {
                    Entry::Occupied(mut e) => *e.get_mut() += amount,
                    Entry::Vacant(e) => {
                        e.insert(*amount);
                    }
                }
            }
            pn_balances.push(PrivateNoteBalance { address, balance: details.balance });
        }

        Self { totals, notes: pn_balances }
    }
}
