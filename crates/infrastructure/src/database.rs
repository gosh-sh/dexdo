// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use std::time::Duration;

use anyhow::Context;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use crate::config::DatabaseSection;

pub async fn build_pool(cfg: &DatabaseSection) -> anyhow::Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(cfg.max_connections)
        .min_connections(cfg.min_connections)
        .acquire_timeout(Duration::from_millis(cfg.connect_timeout_ms))
        .connect(&cfg.url)
        .await
        .context("connect supabase")
}

pub async fn run_migrations(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::migrate!("../../migrations").run(pool).await.context("run migrations")?;
    Ok(())
}
