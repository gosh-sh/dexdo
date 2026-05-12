// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Thin entry point. All bootstrap logic lives in `lib.rs::run` so it
// stays testable and so the binary itself stays a single line.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dodex_api::run().await
}
