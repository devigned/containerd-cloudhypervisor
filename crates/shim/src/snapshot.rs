//! Content-addressable VM snapshot cache.
//!
//! Caches golden VM snapshots (kernel + agent booted, no workload) so that
//! subsequent pods can restore from snapshot instead of cold-booting.
//! Uses userfaultfd-based CoW restore for near-zero memory copy overhead.
//!
//! Cache key = hash of (kernel_path, rootfs_path, vcpus, memory_mb, kernel_args_base).
//! Cache dir = /run/cloudhv/snapshot-cache/{key}/
//!   ├── config.json       (CH VM config)
//!   ├── memory-ranges     (guest RAM — CoW mapped on restore)
//!   └── state.json        (vCPU + device state)

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
