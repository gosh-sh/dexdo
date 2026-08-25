// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
//! Shared test fixtures. `chain_bodies` holds real event bodies captured from chain;
//! the JSON files beside it are read at runtime and need no module.
//!
//! `dead_code` is allowed for the same reason `services/api/tests/common/mod.rs:11`
//! allows it: every integration test binary compiles this module in full but uses only
//! the constants it needs, so the rest are unused *in that binary* and the lint fires.
//! Without this the wave's own gate — `cargo clippy --all-targets -- -D warnings` —
//! fails as soon as the second test file exists.
#![allow(dead_code)]

pub mod chain_bodies;
