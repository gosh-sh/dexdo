mod adapters;
mod client;
mod dapp;
pub mod errors;
mod modules;
mod rate_limiter;
mod services;

#[cfg(not(target_arch = "wasm32"))]
pub use adapters::native::dto::market::MarketInfo;
#[cfg(not(target_arch = "wasm32"))]
pub use adapters::native::dto::market::OracleInfo;
#[cfg(not(target_arch = "wasm32"))]
pub use adapters::native::dto::oracle::EventInfo;
#[cfg(not(target_arch = "wasm32"))]
pub use adapters::native::dto::oracle::OracleEvents;
#[cfg(not(target_arch = "wasm32"))]
pub use adapters::native::dto::order_book::OrderBookDetails;
#[cfg(not(target_arch = "wasm32"))]
pub use adapters::native::dto::order_book::OrderBookShutdownState;
#[cfg(not(target_arch = "wasm32"))]
pub use adapters::native::dto::order_book::OrderInfo;
#[cfg(not(target_arch = "wasm32"))]
pub use adapters::native::dto::order_book::OwnedOrder;
#[cfg(not(target_arch = "wasm32"))]
pub use adapters::native::dto::order_book::OwnedOrders;
#[cfg(not(target_arch = "wasm32"))]
pub use adapters::native::dto::pmp::PmpDetails;
#[cfg(not(target_arch = "wasm32"))]
pub use adapters::native::dto::pmp::PmpShutdownState;
#[cfg(not(target_arch = "wasm32"))]
pub use adapters::native::dto::private_note::AggregatedBalance;
#[cfg(not(target_arch = "wasm32"))]
pub use adapters::native::dto::private_note::DiscoveredNote;
#[cfg(not(target_arch = "wasm32"))]
pub use adapters::native::dto::private_note::PrivateNoteBalance;
#[cfg(not(target_arch = "wasm32"))]
pub use adapters::native::dto::private_note::PrivateNoteDetails;
#[cfg(not(target_arch = "wasm32"))]
pub use adapters::native::dto::private_note::PrivateNoteStakes;
#[cfg(not(target_arch = "wasm32"))]
pub use adapters::native::dto::private_note::ResultOfBlockchainWrite;
#[cfg(not(target_arch = "wasm32"))]
pub use adapters::native::Dex;
#[cfg(feature = "wasm")]
pub use adapters::wasm;
pub use client::DexConfig;
pub use dapp::dex_contract_params;
pub use rate_limiter::maybe_acquire;
pub use rate_limiter::RateLimiter;
#[cfg(not(target_arch = "wasm32"))]
pub use services::halo2;
#[cfg(not(target_arch = "wasm32"))]
pub use services::history::NoteEvent;
#[cfg(not(target_arch = "wasm32"))]
pub use services::history::NotesHistoryPage;
#[cfg(not(target_arch = "wasm32"))]
pub use services::order_book_event;
pub use services::proof;
