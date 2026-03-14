// NOTE: The memory monitor and virtio-mem hot-plug may be unnecessary for
// minimal guest images where CH's lazy mmap (prefault=off) provides natural
// memory efficiency. See issue #47 for simplification discussion.

//! Memory monitor for dynamic VM memory growth and reclaim.
//!
//! Polls guest `/proc/meminfo` via the agent's `GetMemInfo` RPC and
//! resizes the VM when memory pressure changes:
//!
//! - **Growth**: when MemAvailable drops below 20% of MemTotal,
//!   grow memory in 128 MiB steps up to the configured limit.
//! - **Reclaim**: when MemAvailable exceeds 50% of MemTotal for 60s,
//!   shrink memory in 128 MiB steps down to the boot request.
//!
//! Requires virtio-mem hotplug (`hotplug_method = "virtio-mem"`)
//! and a non-zero `hotplug_memory_mb`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use log::{debug, info, warn};
use tokio::sync::watch;

use crate::vm::VmManager;

/// Memory growth/reclaim step size.
const RESIZE_STEP_MB: u64 = 128;

/// Grow when MemAvailable < this fraction of MemTotal.
const GROWTH_THRESHOLD: f64 = 0.20;

/// Shrink when MemAvailable > this fraction of MemTotal for RECLAIM_COOLDOWN.
const RECLAIM_THRESHOLD: f64 = 0.50;

/// How long MemAvailable must stay above RECLAIM_THRESHOLD before shrinking.
const RECLAIM_COOLDOWN: Duration = Duration::from_secs(60);

/// How often to poll guest memory.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Configuration for the memory monitor.
pub struct MemoryMonitorConfig {
    /// Boot memory in bytes (floor — never shrink below this).
    pub boot_memory_bytes: u64,
    /// Maximum memory in bytes (ceiling — never grow above this).
    pub max_memory_bytes: u64,
    /// API socket path for vm.resize calls.
    pub api_socket: PathBuf,
    /// Vsock socket path for agent GetMemInfo calls.
    pub vsock_socket: PathBuf,
    /// Shared directory path — agent writes PSI pressure signals here.
    pub shared_dir: PathBuf,
}

/// Spawn a memory monitor task that runs until the shutdown signal fires.
///
/// Returns a JoinHandle. The monitor stops when `shutdown_rx` receives a value.
pub fn spawn_memory_monitor(
    config: MemoryMonitorConfig,
    mut shutdown_rx: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!(
            "memory monitor started: boot={}MiB max={}MiB",
            config.boot_memory_bytes / (1024 * 1024),
            config.max_memory_bytes / (1024 * 1024)
        );

        let mut current_memory_bytes = config.boot_memory_bytes;
        let mut reclaim_since: Option<tokio::time::Instant> = None;
        let pressure_signal_path = config.shared_dir.join("memory-pressure");

        loop {
            // Check for PSI pressure signal (instant — no waiting).
            // The agent writes this file when /proc/pressure/memory triggers.
            let pressure_event = check_pressure_signal(&pressure_signal_path);

            if pressure_event && current_memory_bytes < config.max_memory_bytes {
                // Immediate growth — don't wait for the 5s poll
                let step = RESIZE_STEP_MB * 1024 * 1024;
                let new_size = (current_memory_bytes + step).min(config.max_memory_bytes);
                info!(
                    "memory monitor: PSI pressure detected! growing {}MiB -> {}MiB",
                    current_memory_bytes / (1024 * 1024),
                    new_size / (1024 * 1024)
                );
                match resize_vm_memory(&config.api_socket, new_size).await {
                    Ok(()) => {
                        current_memory_bytes = new_size;
                        reclaim_since = None;
                    }
                    Err(e) => warn!("memory monitor: pressure resize failed: {e}"),
                }
                // Short sleep to avoid rapid-fire resizes
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }

            // Regular 5s poll cycle
            tokio::select! {
                _ = tokio::time::sleep(POLL_INTERVAL) => {},
                _ = shutdown_rx.changed() => {
                    info!("memory monitor shutting down");
                    return;
                }
            }

            // Query guest memory via agent
            let mem_info = match query_guest_meminfo(&config.vsock_socket).await {
                Ok(info) => info,
                Err(e) => {
                    debug!("memory monitor: agent query failed: {e}");
                    continue;
                }
            };

            let total_kb = mem_info.mem_total_kb;
            let available_kb = mem_info.mem_available_kb;
            if total_kb == 0 {
                continue;
            }

            let available_ratio = available_kb as f64 / total_kb as f64;
            debug!(
                "memory monitor: total={}MiB available={}MiB ({:.0}%) current_vm={}MiB",
                total_kb / 1024,
                available_kb / 1024,
                available_ratio * 100.0,
                current_memory_bytes / (1024 * 1024)
            );

            // Growth: MemAvailable < 20% of MemTotal
            if available_ratio < GROWTH_THRESHOLD && current_memory_bytes < config.max_memory_bytes
            {
                let step = RESIZE_STEP_MB * 1024 * 1024;
                let new_size = (current_memory_bytes + step).min(config.max_memory_bytes);
                info!(
                    "memory monitor: growing VM memory {}MiB -> {}MiB (available={:.0}%)",
                    current_memory_bytes / (1024 * 1024),
                    new_size / (1024 * 1024),
                    available_ratio * 100.0
                );

                match resize_vm_memory(&config.api_socket, new_size).await {
                    Ok(()) => {
                        current_memory_bytes = new_size;
                        reclaim_since = None; // Reset reclaim timer on growth
                    }
                    Err(e) => warn!("memory monitor: resize failed: {e}"),
                }
                continue;
            }

            // Reclaim: MemAvailable > 50% of MemTotal for 60s
            if available_ratio > RECLAIM_THRESHOLD
                && current_memory_bytes > config.boot_memory_bytes
            {
                let now = tokio::time::Instant::now();
                match reclaim_since {
                    None => {
                        reclaim_since = Some(now);
                        debug!("memory monitor: reclaim cooldown started");
                    }
                    Some(since) if now.duration_since(since) >= RECLAIM_COOLDOWN => {
                        let step = RESIZE_STEP_MB * 1024 * 1024;
                        let new_size = current_memory_bytes
                            .saturating_sub(step)
                            .max(config.boot_memory_bytes);
                        info!(
                            "memory monitor: shrinking VM memory {}MiB -> {}MiB (available={:.0}%)",
                            current_memory_bytes / (1024 * 1024),
                            new_size / (1024 * 1024),
                            available_ratio * 100.0
                        );

                        match resize_vm_memory(&config.api_socket, new_size).await {
                            Ok(()) => {
                                current_memory_bytes = new_size;
                                reclaim_since = None; // Reset cooldown
                            }
                            Err(e) => warn!("memory monitor: shrink failed: {e}"),
                        }
                    }
                    _ => {
                        debug!("memory monitor: reclaim cooldown in progress");
                    }
                }
            } else {
                reclaim_since = None;
            }
        }
    })
}

/// Guest memory info from /proc/meminfo.
struct GuestMemInfo {
    mem_total_kb: u64,
    mem_available_kb: u64,
}

/// Query the agent for guest memory stats.
async fn query_guest_meminfo(vsock_socket: &Path) -> anyhow::Result<GuestMemInfo> {
    let vsock_client = crate::vsock::VsockClient::new(vsock_socket);
    let (agent, _health) = vsock_client.connect_ttrpc().await?;

    let ctx = ttrpc::context::with_duration(Duration::from_secs(2));
    let resp = agent
        .get_mem_info(ctx, &cloudhv_proto::GetMemInfoRequest::new())
        .await
        .map_err(|e| anyhow::anyhow!("GetMemInfo RPC failed: {e}"))?;

    Ok(GuestMemInfo {
        mem_total_kb: resp.mem_total_kb,
        mem_available_kb: resp.mem_available_kb,
    })
}

/// Resize VM memory via the Cloud Hypervisor API.
async fn resize_vm_memory(api_socket: &Path, desired_bytes: u64) -> anyhow::Result<()> {
    VmManager::api_request_to_socket(
        api_socket,
        "PUT",
        "/api/v1/vm.resize",
        Some(&format!(r#"{{"desired_ram":{desired_bytes}}}"#)),
    )
    .await
    .map(|_| ())
}

/// Check for a PSI pressure signal file from the agent.
/// Returns true if the signal exists (and removes it to acknowledge).
fn check_pressure_signal(signal_path: &Path) -> bool {
    if signal_path.exists() {
        // Remove the signal file to acknowledge it
        let _ = std::fs::remove_file(signal_path);
        true
    } else {
        false
    }
}
