use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use log::info;

mod api;
mod config;
mod pool;
pub mod shadow;
mod vm_lifecycle;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let config_path = std::env::args()
        .nth(1)
        .or_else(|| {
            std::env::args()
                .find(|a| a.starts_with("--config="))
                .map(|a| a[9..].to_string())
        })
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/opt/cloudhv/daemon.json"));

    info!(
        "cloudhv-sandbox-daemon starting (version {})",
        env!("CARGO_PKG_VERSION")
    );
    info!("config: {}", config_path.display());

    let config = config::DaemonConfig::load(&config_path)?;
    info!(
        "pool_size: {}, memory: {}MiB, vcpus: {}",
        config.pool_size, config.default_memory_mb, config.default_vcpus
    );

    // Initialize pool (creates base snapshot if needed, fills pool)
    let pool = pool::Pool::new(config.clone());
    pool.initialize().await?;

    // Initialize snapshot manager (for warm workload snapshots)
    let snapshots = shadow::SnapshotManager::new(config.clone(), Arc::clone(&pool));

    // Start API server
    let server = api::ApiServer::new(config, pool, snapshots);
    server.serve().await
}
