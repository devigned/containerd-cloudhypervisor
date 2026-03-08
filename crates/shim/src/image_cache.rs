use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use log::{debug, info};

/// Manages shared OCI image layers across VMs via the virtio-fs shared directory.
///
/// When multiple containers use the same base image, this cache ensures
/// the image layers are stored once on the host and shared read-only
/// into each VM via virtio-fs. Each container gets a thin overlayfs
/// layer on top.
///
/// Host layout:
///   /run/cloudhv/layers/<digest>/    — extracted layer content (shared, read-only)
///   /run/cloudhv/<vm_id>/shared/io/  — per-container I/O
///
/// Guest layout:
///   /containers/layers/<digest>/     — mounted via virtio-fs
///   /containers/<container_id>/      — overlayfs: layers + writable upper
#[allow(dead_code)]
pub struct ImageLayerCache {
    /// Base directory for cached layers on the host.
    cache_dir: PathBuf,
    /// Tracks which layers are cached: digest -> host path.
    layers: HashMap<String, PathBuf>,
    /// Reference count per layer (how many containers use it).
    refcounts: HashMap<String, usize>,
}

impl ImageLayerCache {
    /// Create a new image layer cache.
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            layers: HashMap::new(),
            refcounts: HashMap::new(),
        }
    }

    /// Create from the default state directory.
    #[allow(dead_code)]
    pub fn default_cache() -> Self {
        Self::new(PathBuf::from(cloudhv_common::RUNTIME_STATE_DIR).join("layers"))
    }

    /// Ensure a layer is cached on the host. Returns the host path.
    ///
    /// If the layer is already cached, increments its reference count.
    /// If not, creates the directory (caller must populate it).
    #[allow(dead_code)]
    pub fn ensure_layer(&mut self, digest: &str) -> Result<PathBuf> {
        if let Some(path) = self.layers.get(digest) {
            *self.refcounts.entry(digest.to_string()).or_insert(0) += 1;
            debug!(
                "layer cache hit: {} (refs={})",
                digest, self.refcounts[digest]
            );
            return Ok(path.clone());
        }

        let layer_dir = self.cache_dir.join(sanitize_digest(digest));
        std::fs::create_dir_all(&layer_dir)
            .with_context(|| format!("failed to create layer dir: {}", layer_dir.display()))?;

        info!("layer cached: {} at {}", digest, layer_dir.display());
        self.layers.insert(digest.to_string(), layer_dir.clone());
        self.refcounts.insert(digest.to_string(), 1);
        Ok(layer_dir)
    }

    /// Release a reference to a layer. Removes from disk when refcount reaches 0.
    #[allow(dead_code)]
    pub fn release_layer(&mut self, digest: &str) {
        if let Some(count) = self.refcounts.get_mut(digest) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.refcounts.remove(digest);
                if let Some(path) = self.layers.remove(digest) {
                    info!("layer evicted: {} (refcount=0)", digest);
                    let _ = std::fs::remove_dir_all(&path);
                }
            } else {
                debug!("layer release: {} (refs={})", digest, count);
            }
        }
    }

    /// Check if a layer is already cached.
    #[allow(dead_code)]
    pub fn is_cached(&self, digest: &str) -> bool {
        self.layers.contains_key(digest)
    }

    /// Get the host path for a cached layer.
    #[allow(dead_code)]
    pub fn layer_path(&self, digest: &str) -> Option<&Path> {
        self.layers.get(digest).map(|p| p.as_path())
    }

    /// Number of cached layers.
    #[allow(dead_code)]
    pub fn cached_count(&self) -> usize {
        self.layers.len()
    }

    /// Clean up all cached layers.
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        info!("clearing image layer cache ({} layers)", self.layers.len());
        for (_, path) in self.layers.drain() {
            let _ = std::fs::remove_dir_all(&path);
        }
        self.refcounts.clear();
    }
}

/// Sanitize a digest string for use as a directory name.
/// Replaces ':' with '_' (e.g., "sha256:abc123" -> "sha256_abc123").
#[allow(dead_code)]
fn sanitize_digest(digest: &str) -> String {
    digest.replace(':', "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_digest() {
        assert_eq!(sanitize_digest("sha256:abc123def"), "sha256_abc123def");
        assert_eq!(sanitize_digest("abc123"), "abc123");
    }

    #[test]
    fn test_layer_cache_operations() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = ImageLayerCache::new(dir.path().to_path_buf());

        assert_eq!(cache.cached_count(), 0);
        assert!(!cache.is_cached("sha256:abc"));

        // Ensure a layer
        let path = cache.ensure_layer("sha256:abc").unwrap();
        assert!(path.exists());
        assert!(cache.is_cached("sha256:abc"));
        assert_eq!(cache.cached_count(), 1);

        // Ensure again — should be a cache hit
        let path2 = cache.ensure_layer("sha256:abc").unwrap();
        assert_eq!(path, path2);

        // Release once — still cached (refcount=1)
        cache.release_layer("sha256:abc");
        assert!(cache.is_cached("sha256:abc"));

        // Release again — evicted (refcount=0)
        cache.release_layer("sha256:abc");
        assert!(!cache.is_cached("sha256:abc"));
        assert_eq!(cache.cached_count(), 0);
    }

    #[test]
    fn test_cache_clear() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = ImageLayerCache::new(dir.path().to_path_buf());

        cache.ensure_layer("sha256:aaa").unwrap();
        cache.ensure_layer("sha256:bbb").unwrap();
        assert_eq!(cache.cached_count(), 2);

        cache.clear();
        assert_eq!(cache.cached_count(), 0);
    }
}
