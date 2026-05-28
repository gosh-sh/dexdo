#[cfg(not(target_arch = "wasm32"))]
pub mod native;
#[cfg(feature = "wasm")]
pub mod wasm;
