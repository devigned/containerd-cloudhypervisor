//! Pod annotation handling for VM resource configuration.
//!
//! Supports both Kata-compatible and native annotation prefixes:
//!
//! | Prefix | Priority |
//! |--------|----------|
//! | `io.cloudhv.` | **Primary** — always wins if present |
//! | `io.katacontainers.` | **Fallback** — used if no `io.cloudhv.` equivalent |
//!
//! This allows Kata users to migrate without changing annotations, while
//! providing a native namespace for Cloud Hypervisor-specific settings.
//!
//! ## Supported Annotations
//!
//! | Suffix | Type | Description |
//! |--------|------|-------------|
//! | `config.hypervisor.default_memory` | u64 (MiB) | VM memory size (min 128) |
//! | `config.hypervisor.default_vcpus` | u32 | VM vCPU count |
//! | `config.hypervisor.default_max_vcpus` | u32 | Max vCPUs for hotplug |
//! | `config.hypervisor.kernel_params` | string | Extra kernel boot parameters |
//! | `config.hypervisor.enable_virtio_mem` | bool | Use virtio-mem for memory hotplug |
//!
//! ## Examples
//!
//! ```yaml
//! # Kata-compatible (works as fallback)
//! io.katacontainers.config.hypervisor.default_memory: "2048"
//!
//! # Native (takes priority)
//! io.cloudhv.config.hypervisor.default_memory: "2048"
//!
//! # Both present — io.cloudhv wins
//! io.katacontainers.config.hypervisor.default_vcpus: "2"
//! io.cloudhv.config.hypervisor.default_vcpus: "4"  # ← this wins
//! ```

use std::collections::HashMap;

use log::{info, warn};

use cloudhv_common::types::RuntimeConfig;

const CLOUDHV_PREFIX: &str = "io.cloudhv.";
const KATA_PREFIX: &str = "io.katacontainers.";

// Annotation suffixes (shared between io.cloudhv. and io.katacontainers.)
const SUFFIX_MEMORY: &str = "config.hypervisor.default_memory";
const SUFFIX_MEMORY_LIMIT: &str = "config.hypervisor.memory_limit";
const SUFFIX_VCPUS: &str = "config.hypervisor.default_vcpus";
const SUFFIX_MAX_VCPUS: &str = "config.hypervisor.default_max_vcpus";
const SUFFIX_KERNEL_PARAMS: &str = "config.hypervisor.kernel_params";
const SUFFIX_VIRTIO_MEM: &str = "config.hypervisor.enable_virtio_mem";

/// Minimum VM memory in MiB (matches Kata's MinHypervisorMemory).
const MIN_MEMORY_MB: u64 = 128;

/// Read an annotation value, preferring `io.cloudhv.` over `io.katacontainers.`.
fn resolve_annotation<'a>(
    annotations: &'a HashMap<String, String>,
    suffix: &str,
) -> Option<&'a str> {
    let cloudhv_key = format!("{CLOUDHV_PREFIX}{suffix}");
    let kata_key = format!("{KATA_PREFIX}{suffix}");

    annotations
        .get(&cloudhv_key)
        .or_else(|| annotations.get(&kata_key))
        .map(|s| s.as_str())
}

/// Apply pod annotations to override the runtime config.
///
/// Reads annotations from the OCI spec and overrides VM resource settings.
/// Invalid values are logged and ignored (preserving the default).
pub fn apply_annotations(
    mut config: RuntimeConfig,
    annotations: &HashMap<String, String>,
) -> RuntimeConfig {
    // Memory (MiB)
    if let Some(val) = resolve_annotation(annotations, SUFFIX_MEMORY) {
        match val.parse::<u64>() {
            Ok(mb) if mb >= MIN_MEMORY_MB => {
                info!(
                    "annotation: default_memory_mb {} -> {}",
                    config.default_memory_mb, mb
                );
                config.default_memory_mb = mb;
            }
            Ok(mb) => warn!(
                "annotation: default_memory={} below minimum {}MiB, ignored",
                mb, MIN_MEMORY_MB
            ),
            Err(_) => warn!(
                "annotation: default_memory={:?} is not a valid number, ignored",
                val
            ),
        }
    }

    // Memory limit (MiB) — sets hotplug headroom for dynamic growth.
    // When limit > request, the VM boots with request as initial memory and
    // can grow up to limit via virtio-mem hotplug.
    if let Some(val) = resolve_annotation(annotations, SUFFIX_MEMORY_LIMIT) {
        match val.parse::<u64>() {
            Ok(limit_mb) if limit_mb > config.default_memory_mb => {
                let headroom = limit_mb - config.default_memory_mb;
                info!(
                    "annotation: memory_limit={}MiB, hotplug_memory_mb={}MiB (headroom for growth)",
                    limit_mb, headroom
                );
                config.hotplug_memory_mb = headroom;
                // Auto-select virtio-mem for bidirectional resize
                if config.hotplug_method == "acpi" {
                    config.hotplug_method = "virtio-mem".to_string();
                    info!("annotation: auto-selected virtio-mem for memory growth/reclaim");
                }
            }
            Ok(limit_mb) if limit_mb > 0 => {
                info!(
                    "annotation: memory_limit={}MiB <= default_memory={}MiB, no hotplug needed",
                    limit_mb, config.default_memory_mb
                );
            }
            Ok(_) => {}
            Err(_) => warn!(
                "annotation: memory_limit={:?} is not a valid number, ignored",
                val
            ),
        }
    }

    // vCPUs
    if let Some(val) = resolve_annotation(annotations, SUFFIX_VCPUS) {
        match val.parse::<u32>() {
            Ok(n) if n > 0 => {
                info!(
                    "annotation: default_vcpus {} -> {}",
                    config.default_vcpus, n
                );
                config.default_vcpus = n;
            }
            Ok(_) => warn!("annotation: default_vcpus=0 is not valid, ignored"),
            Err(_) => warn!(
                "annotation: default_vcpus={:?} is not a valid number, ignored",
                val
            ),
        }
    }

    // Max vCPUs (for hotplug)
    if let Some(val) = resolve_annotation(annotations, SUFFIX_MAX_VCPUS) {
        if let Ok(n) = val.parse::<u32>() {
            if n > 0 && n >= config.default_vcpus {
                info!("annotation: max_vcpus -> {}", n);
                // max_vcpus is computed dynamically in create_and_boot_vm from
                // available_parallelism, but we can ensure default_vcpus respects it
                // by capping default_vcpus if needed. The actual max_vcpus is set
                // at VM creation time.
            }
        }
    }

    // Extra kernel parameters (appended, not replaced)
    if let Some(val) = resolve_annotation(annotations, SUFFIX_KERNEL_PARAMS) {
        if !val.is_empty() {
            info!("annotation: appending kernel_params: {}", val);
            config.kernel_args.push(' ');
            config.kernel_args.push_str(val);
        }
    }

    // virtio-mem for memory hotplug
    if let Some(val) = resolve_annotation(annotations, SUFFIX_VIRTIO_MEM) {
        match val.to_lowercase().as_str() {
            "true" | "1" | "yes" => {
                info!("annotation: hotplug_method -> virtio-mem");
                config.hotplug_method = "virtio-mem".to_string();
            }
            "false" | "0" | "no" => {
                config.hotplug_method = "acpi".to_string();
            }
            _ => warn!(
                "annotation: enable_virtio_mem={:?} is not a valid boolean, ignored",
                val
            ),
        }
    }

    config
}

/// Extract annotations from a parsed OCI spec JSON.
///
/// Only collects annotations with `io.cloudhv.` or `io.katacontainers.` prefixes.
pub fn annotations_from_spec(spec: &serde_json::Value) -> HashMap<String, String> {
    let mut result = HashMap::new();
    if let Some(obj) = spec.get("annotations").and_then(|a| a.as_object()) {
        for (key, val) in obj {
            if let Some(s) = val.as_str() {
                if key.starts_with(CLOUDHV_PREFIX) || key.starts_with(KATA_PREFIX) {
                    result.insert(key.clone(), s.to_string());
                }
            }
        }
    }
    result
}

/// Extract memory request and limit from the OCI spec's linux.resources.
///
/// Kubernetes sets `linux.resources.memory.limit` (in bytes) from
/// `resources.limits.memory` in the pod spec. Returns (request_mb, limit_mb).
/// If not set, returns None for that field.
pub fn memory_resources_from_spec(spec: &serde_json::Value) -> (Option<u64>, Option<u64>) {
    let limit_bytes = spec
        .pointer("/linux/resources/memory/limit")
        .and_then(|v| v.as_i64())
        .filter(|&v| v > 0);

    // OCI spec doesn't have a "request" field — only limit and reservation.
    // Reservation maps to Kubernetes memory requests.
    let request_bytes = spec
        .pointer("/linux/resources/memory/reservation")
        .and_then(|v| v.as_i64())
        .filter(|&v| v > 0);

    let to_mb = |bytes: i64| -> u64 { (bytes as u64) / (1024 * 1024) };

    (request_bytes.map(to_mb), limit_bytes.map(to_mb))
}

/// Apply OCI resource limits to the runtime config.
///
/// When a memory limit exceeds the configured boot memory, automatically
/// enables virtio-mem hotplug with headroom up to the limit.
pub fn apply_resource_limits(
    mut config: RuntimeConfig,
    request_mb: Option<u64>,
    limit_mb: Option<u64>,
) -> RuntimeConfig {
    // If request is set and valid, use it as boot memory
    if let Some(req) = request_mb {
        if req >= MIN_MEMORY_MB {
            info!(
                "resource request: default_memory_mb {} -> {}",
                config.default_memory_mb, req
            );
            config.default_memory_mb = req;
        }
    }

    // If limit exceeds boot memory, enable hotplug for growth
    if let Some(limit) = limit_mb {
        if limit > config.default_memory_mb {
            let headroom = limit - config.default_memory_mb;
            if config.hotplug_memory_mb < headroom {
                info!(
                    "resource limit: hotplug_memory_mb {} -> {} (limit={}MiB, boot={}MiB)",
                    config.hotplug_memory_mb, headroom, limit, config.default_memory_mb
                );
                config.hotplug_memory_mb = headroom;
                // Auto-select virtio-mem for bidirectional resize
                if config.hotplug_method == "acpi" {
                    config.hotplug_method = "virtio-mem".to_string();
                    info!("auto-selected virtio-mem for dynamic memory growth/reclaim");
                }
            }
        }
    }

    config
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> RuntimeConfig {
        RuntimeConfig {
            cloud_hypervisor_binary: String::new(),
            virtiofsd_binary: String::new(),
            kernel_path: String::new(),
            rootfs_path: String::new(),
            default_vcpus: 1,
            default_memory_mb: 512,
            vsock_port: 0,
            agent_startup_timeout_secs: 10,
            kernel_args: "console=hvc0".to_string(),
            debug: false,
            pool_size: 0,
            max_containers_per_vm: 1,
            hotplug_memory_mb: 0,
            hotplug_method: "acpi".to_string(),
            tpm_enabled: false,
        }
    }

    #[test]
    fn test_cloudhv_annotation_sets_memory() {
        let mut ann = HashMap::new();
        ann.insert(
            "io.cloudhv.config.hypervisor.default_memory".into(),
            "2048".into(),
        );
        let config = apply_annotations(default_config(), &ann);
        assert_eq!(config.default_memory_mb, 2048);
    }

    #[test]
    fn test_kata_annotation_sets_memory() {
        let mut ann = HashMap::new();
        ann.insert(
            "io.katacontainers.config.hypervisor.default_memory".into(),
            "1024".into(),
        );
        let config = apply_annotations(default_config(), &ann);
        assert_eq!(config.default_memory_mb, 1024);
    }

    #[test]
    fn test_cloudhv_wins_over_kata() {
        let mut ann = HashMap::new();
        ann.insert(
            "io.katacontainers.config.hypervisor.default_memory".into(),
            "1024".into(),
        );
        ann.insert(
            "io.cloudhv.config.hypervisor.default_memory".into(),
            "4096".into(),
        );
        let config = apply_annotations(default_config(), &ann);
        assert_eq!(config.default_memory_mb, 4096);
    }

    #[test]
    fn test_vcpu_annotation() {
        let mut ann = HashMap::new();
        ann.insert(
            "io.cloudhv.config.hypervisor.default_vcpus".into(),
            "4".into(),
        );
        let config = apply_annotations(default_config(), &ann);
        assert_eq!(config.default_vcpus, 4);
    }

    #[test]
    fn test_kata_vcpu_fallback() {
        let mut ann = HashMap::new();
        ann.insert(
            "io.katacontainers.config.hypervisor.default_vcpus".into(),
            "8".into(),
        );
        let config = apply_annotations(default_config(), &ann);
        assert_eq!(config.default_vcpus, 8);
    }

    #[test]
    fn test_no_annotations_preserves_defaults() {
        let config = apply_annotations(default_config(), &HashMap::new());
        assert_eq!(config.default_memory_mb, 512);
        assert_eq!(config.default_vcpus, 1);
    }

    #[test]
    fn test_invalid_value_ignored() {
        let mut ann = HashMap::new();
        ann.insert(
            "io.cloudhv.config.hypervisor.default_memory".into(),
            "not-a-number".into(),
        );
        let config = apply_annotations(default_config(), &ann);
        assert_eq!(config.default_memory_mb, 512);
    }

    #[test]
    fn test_memory_below_minimum_ignored() {
        let mut ann = HashMap::new();
        ann.insert(
            "io.cloudhv.config.hypervisor.default_memory".into(),
            "64".into(),
        );
        let config = apply_annotations(default_config(), &ann);
        assert_eq!(config.default_memory_mb, 512); // unchanged
    }

    #[test]
    fn test_zero_vcpus_ignored() {
        let mut ann = HashMap::new();
        ann.insert(
            "io.cloudhv.config.hypervisor.default_vcpus".into(),
            "0".into(),
        );
        let config = apply_annotations(default_config(), &ann);
        assert_eq!(config.default_vcpus, 1); // unchanged
    }

    #[test]
    fn test_kernel_params_appended() {
        let mut ann = HashMap::new();
        ann.insert(
            "io.cloudhv.config.hypervisor.kernel_params".into(),
            "quiet loglevel=0".into(),
        );
        let config = apply_annotations(default_config(), &ann);
        assert_eq!(config.kernel_args, "console=hvc0 quiet loglevel=0");
    }

    #[test]
    fn test_virtio_mem_annotation() {
        let mut ann = HashMap::new();
        ann.insert(
            "io.katacontainers.config.hypervisor.enable_virtio_mem".into(),
            "true".into(),
        );
        let config = apply_annotations(default_config(), &ann);
        assert_eq!(config.hotplug_method, "virtio-mem");
    }

    #[test]
    fn test_annotations_from_spec() {
        let spec = serde_json::json!({
            "annotations": {
                "io.cloudhv.config.hypervisor.default_memory": "2048",
                "io.katacontainers.config.hypervisor.default_vcpus": "4",
                "io.kubernetes.cri.container-type": "sandbox",
                "unrelated.annotation": "ignored"
            }
        });
        let annotations = annotations_from_spec(&spec);
        assert_eq!(annotations.len(), 2);
        assert_eq!(
            annotations["io.cloudhv.config.hypervisor.default_memory"],
            "2048"
        );
        assert_eq!(
            annotations["io.katacontainers.config.hypervisor.default_vcpus"],
            "4"
        );
    }

    #[test]
    fn test_multiple_annotations_combined() {
        let mut ann = HashMap::new();
        ann.insert(
            "io.cloudhv.config.hypervisor.default_memory".into(),
            "2048".into(),
        );
        ann.insert(
            "io.katacontainers.config.hypervisor.default_vcpus".into(),
            "4".into(),
        );
        ann.insert(
            "io.cloudhv.config.hypervisor.kernel_params".into(),
            "debug".into(),
        );
        let config = apply_annotations(default_config(), &ann);
        assert_eq!(config.default_memory_mb, 2048);
        assert_eq!(config.default_vcpus, 4);
        assert!(config.kernel_args.contains("debug"));
    }

    #[test]
    fn test_memory_limit_enables_hotplug() {
        let mut ann = HashMap::new();
        ann.insert(
            "io.cloudhv.config.hypervisor.default_memory".into(),
            "128".into(),
        );
        ann.insert(
            "io.cloudhv.config.hypervisor.memory_limit".into(),
            "1024".into(),
        );
        let config = apply_annotations(default_config(), &ann);
        assert_eq!(config.default_memory_mb, 128);
        assert_eq!(config.hotplug_memory_mb, 896);
        assert_eq!(config.hotplug_method, "virtio-mem");
    }

    #[test]
    fn test_memory_limit_below_request_no_hotplug() {
        let mut ann = HashMap::new();
        ann.insert(
            "io.cloudhv.config.hypervisor.default_memory".into(),
            "512".into(),
        );
        ann.insert(
            "io.cloudhv.config.hypervisor.memory_limit".into(),
            "256".into(),
        );
        let config = apply_annotations(default_config(), &ann);
        assert_eq!(config.default_memory_mb, 512);
        assert_eq!(config.hotplug_memory_mb, 0);
    }

    #[test]
    fn test_resource_limits_from_spec() {
        let spec = serde_json::json!({
            "linux": {
                "resources": {
                    "memory": {
                        "limit": 1073741824_i64,
                        "reservation": 134217728_i64
                    }
                }
            }
        });
        let (req, lim) = memory_resources_from_spec(&spec);
        assert_eq!(req, Some(128));
        assert_eq!(lim, Some(1024));
    }

    #[test]
    fn test_apply_resource_limits_enables_hotplug() {
        let config = apply_resource_limits(default_config(), Some(128), Some(1024));
        assert_eq!(config.default_memory_mb, 128);
        assert_eq!(config.hotplug_memory_mb, 896);
        assert_eq!(config.hotplug_method, "virtio-mem");
    }

    #[test]
    fn test_apply_resource_limits_no_limit() {
        let config = apply_resource_limits(default_config(), Some(256), None);
        assert_eq!(config.default_memory_mb, 256);
        assert_eq!(config.hotplug_memory_mb, 0);
    }

    #[test]
    fn test_apply_resource_limits_limit_equals_request() {
        let config = apply_resource_limits(default_config(), Some(512), Some(512));
        assert_eq!(config.default_memory_mb, 512);
        assert_eq!(config.hotplug_memory_mb, 0);
    }
}
