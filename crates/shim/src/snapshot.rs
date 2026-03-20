//! # Warm Workload Snapshot Cache
//!
//! Content-addressable cache of fully-warmed VM snapshots. Each snapshot
//! captures a VM with the kernel booted, agent running, AND the container
//! workload fully started (e.g. Python server listening on port 8888).
//!
//! ## Why "Warm" Snapshots?
//!
//! Some workloads have expensive initialization: Python imports, JIT
//! compilation, model loading, server startup. A Python uvicorn server
//! takes ~15s to start on 1 vCPU. By snapshotting AFTER the server is
//! ready, restored VMs get an instantly-warm workload — 15s → ~200ms.
//!
//! ## How Networking Survives Restore
//!
//! The snapshot is stripped of pod-specific networking (TAP device, IP
//! address). On restore:
//!
//! 1. A new TAP device is hot-plugged via `vm.add-net`
//! 2. The guest IP is configured via the agent's `ConfigureNetwork` RPC
//! 3. The workload (e.g. uvicorn) binds `0.0.0.0:<port>`, which accepts
//!    on ALL interfaces — including the newly-configured eth0
//! 4. No socket rebind needed: `0.0.0.0` is interface-agnostic
//!
//! ## Cache Structure
//!
//! ```text
//! /run/cloudhv/snapshot-cache/
//!   {key}/
//!     config.json       CH VM config (networking stripped)
//!     memory-ranges     Guest RAM (CoW-mapped via userfaultfd on restore)
//!     state.json        vCPU + device state
//!   {key}.lock          flock for serializing snapshot creation
//! ```
//!
//! Cache key = hash of (kernel, rootfs, vcpus, memory, container_image).
//! Same workload image → same snapshot → instant restore.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::instance::stable_hash_hex;
use cloudhv_common::types::RuntimeConfig;

/// Base directory for cached snapshots (tmpfs, fast, lost on reboot).
const SNAPSHOT_CACHE_DIR: &str = "/run/cloudhv/snapshot-cache";

/// Compute a content-addressable cache key for a VM snapshot.
///
/// The key is based on the immutable inputs that define the VM's boot state:
/// kernel binary, guest rootfs, vCPU count, and memory size.
/// Two VMs with the same key will produce identical boot states.
pub fn snapshot_cache_key(config: &RuntimeConfig) -> String {
    let input = format!(
        "{}:{}:{}:{}",
        config.kernel_path, config.rootfs_path, config.default_vcpus, config.default_memory_mb,
    );
    stable_hash_hex(&input)
}

/// Return the cache directory for a given snapshot key.
pub fn snapshot_cache_dir(key: &str) -> PathBuf {
    PathBuf::from(SNAPSHOT_CACHE_DIR).join(key)
}

/// Check if a valid snapshot exists in the cache.
pub fn snapshot_cache_hit(key: &str) -> bool {
    let dir = snapshot_cache_dir(key);
    dir.join("config.json").exists()
        && dir.join("memory-ranges").exists()
        && dir.join("state.json").exists()
}

/// Acquire an exclusive lock for snapshot creation.
/// Returns the lock file (held until dropped).
/// If the lock is already held, blocks until available.
pub fn snapshot_cache_lock(key: &str) -> Result<std::fs::File> {
    let cache_dir = PathBuf::from(SNAPSHOT_CACHE_DIR);
    std::fs::create_dir_all(&cache_dir).context("create snapshot cache dir")?;

    let lock_path = cache_dir.join(format!("{key}.lock"));
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .context("open snapshot lock")?;

    use std::os::unix::io::AsRawFd;
    loop {
        let rc = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) };
        if rc == 0 {
            break;
        }
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err).context("flock snapshot lock");
    }

    Ok(lock_file)
}

/// Store a snapshot in the cache. The source directory is renamed into
/// the cache atomically. If the cache entry already exists (race with
/// another shim), this is a no-op.
pub fn snapshot_cache_store(key: &str, source_dir: &Path) -> Result<()> {
    let dest = snapshot_cache_dir(key);
    if dest.exists() {
        // Another process already cached it
        log::info!("snapshot cache: {key} already exists, skipping store");
        return Ok(());
    }

    // Rename atomically (same filesystem — both on /run tmpfs)
    std::fs::rename(source_dir, &dest)
        .or_else(|_| {
            // If rename fails (cross-device), fall back to copy
            copy_dir_all(source_dir, &dest)
        })
        .with_context(|| format!("store snapshot {key}"))?;

    log::info!("snapshot cached: {}", dest.display());
    Ok(())
}

/// Create a per-VM working copy of a cached snapshot with patched vsock config.
/// Hardlinks memory-ranges and state.json (CoW-friendly), only rewrites config.json.
pub fn prepare_snapshot_for_vm(
    cache_key: &str,
    vm_state_dir: &Path, // e.g. /run/cloudhv/<vm_id>/
    new_cid: u64,
    new_vsock_socket: &Path,
) -> Result<PathBuf> {
    let cache_dir = snapshot_cache_dir(cache_key);
    let work_dir = vm_state_dir.join("snapshot");
    std::fs::create_dir_all(&work_dir)?;

    // Hardlink the large files (memory-ranges can be 512MB+ — don't copy)
    for name in &["memory-ranges", "state.json"] {
        let src = cache_dir.join(name);
        let dst = work_dir.join(name);
        std::fs::hard_link(&src, &dst)
            .or_else(|_| std::fs::copy(&src, &dst).map(|_| ()))
            .with_context(|| format!("link {name} from cache"))?;
    }

    // Read, patch, and write config.json
    let config_str = std::fs::read_to_string(cache_dir.join("config.json"))
        .context("read snapshot config.json")?;
    let mut config: serde_json::Value =
        serde_json::from_str(&config_str).context("parse snapshot config.json")?;

    // Patch vsock CID and socket path
    if let Some(vsock) = config.pointer_mut("/vsock") {
        if let Some(obj) = vsock.as_object_mut() {
            obj.insert("cid".to_string(), serde_json::json!(new_cid));
            obj.insert(
                "socket".to_string(),
                serde_json::json!(new_vsock_socket.to_string_lossy()),
            );
        }
    }

    // Patch serial console file path — CH opens this file on restore and
    // fails with ENOENT if it points to the old VM's state directory.
    let console_path = vm_state_dir.join("console.log");
    // Create the file so CH can open it
    let _ = std::fs::File::create(&console_path);
    if let Some(serial) = config.pointer_mut("/serial") {
        if let Some(obj) = serial.as_object_mut() {
            if obj.get("mode").and_then(|v| v.as_str()) == Some("File") {
                obj.insert(
                    "file".to_string(),
                    serde_json::json!(console_path.to_string_lossy()),
                );
            }
        }
    }

    // Remove container-specific disks from snapshot config.
    // Keep only disk 0 (guest rootfs at /opt/cloudhv/rootfs.erofs or rootfs.ext4).
    // Container rootfs disks are hot-plugged after restore.
    if let Some(disks) = config.pointer_mut("/disks") {
        if let Some(arr) = disks.as_array_mut() {
            arr.retain(|d| {
                d.get("path")
                    .and_then(|p| p.as_str())
                    .map(|p| p.starts_with("/opt/cloudhv/"))
                    .unwrap_or(false)
            });
        }
    }

    std::fs::write(
        work_dir.join("config.json"),
        serde_json::to_string_pretty(&config)?,
    )
    .context("write patched config.json")?;

    log::info!(
        "prepared snapshot for VM: cid={new_cid} socket={} dir={}",
        new_vsock_socket.display(),
        work_dir.display()
    );
    Ok(work_dir)
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), std::io::Error> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_file() {
            std::fs::copy(entry.path(), dst.join(entry.file_name()))?;
        }
    }
    Ok(())
}
