use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result};
#[cfg(target_os = "linux")]
use log::debug;
use log::{error, info, warn};
use tokio::process::Command;
use tokio::sync::watch;

/// Exit status reported through the watch channel when a container stops.
#[derive(Debug, Clone)]
pub struct ExitStatus {
    pub code: i32,
    pub exited_at: String,
}

/// Volume information passed from the shim to the agent.
pub struct VolumeInfo {
    pub destination: String,
    pub source: String,
    pub readonly: bool,
    pub is_block: bool,
    pub fs_type: String,
}

/// Tracks the state of a container managed by the agent.
#[derive(Debug)]
struct Container {
    pid: Option<u32>,
    exit_code: Option<i32>,
    state: ContainerState,
    stdout_path: Option<String>,
    stderr_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ContainerState {
    Created,
    Running,
    Stopped,
}

/// Manages container lifecycle via crun.
pub struct ContainerManager {
    containers: HashMap<String, Container>,
    /// Block devices already known (to detect newly hot-plugged ones).
    known_disks: std::collections::HashSet<String>,
    /// Watch receivers for container exit notifications.
    exit_receivers: HashMap<String, watch::Receiver<Option<ExitStatus>>>,
}

impl Default for ContainerManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ContainerManager {
    pub fn new() -> Self {
        let known_disks = Self::scan_block_devices();
        Self {
            containers: HashMap::new(),
            known_disks,
            exit_receivers: HashMap::new(),
        }
    }

    /// Scan /sys/block for current virtio block devices.
    fn scan_block_devices() -> std::collections::HashSet<String> {
        std::fs::read_dir("/sys/block")
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|name| name.starts_with("vd"))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Wait for a newly hot-plugged block device to appear, then mount it.
    /// Returns the device path (e.g., "/dev/vdc").
    async fn discover_and_mount_new_disk(
        &mut self,
        _mount_point: &std::path::Path,
    ) -> Result<String> {
        // Poll for a new vdX device to appear (ACPI hot-plug detection)
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let new_disk = loop {
            let current = Self::scan_block_devices();
            let new_devices: Vec<_> = current.difference(&self.known_disks).cloned().collect();
            if let Some(dev) = new_devices.first() {
                self.known_disks.insert(dev.clone());
                break format!("/dev/{dev}");
            }
            if tokio::time::Instant::now() > deadline {
                anyhow::bail!(
                    "timed out waiting for hot-plugged disk (known: {:?})",
                    self.known_disks
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };

        info!("discovered new disk: {}", new_disk);

        // Mount the disk — retry as the device may not be immediately ready
        #[cfg(target_os = "linux")]
        {
            use nix::mount::{mount, MsFlags};
            for attempt in 1..=20 {
                match mount(
                    Some(new_disk.as_str()),
                    _mount_point,
                    Some("ext4"),
                    MsFlags::empty(),
                    None::<&str>,
                ) {
                    Ok(()) => return Ok(new_disk),
                    Err(e) if attempt < 20 => {
                        debug!("mount attempt {attempt} failed: {e}");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    Err(e) => {
                        anyhow::bail!("mount {} at {}: {e}", new_disk, _mount_point.display())
                    }
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = _mount_point;

        anyhow::bail!("failed to mount disk after retries")
    }

    /// Discover a newly hot-plugged block device by scanning /sys/block.
    /// Returns the device path (e.g., "/dev/vdc") if a new device is found.
    fn discover_block_device(&mut self) -> Option<String> {
        let current = Self::scan_block_devices();
        let new_devices: Vec<_> = current.difference(&self.known_disks).cloned().collect();
        if let Some(dev) = new_devices.first() {
            self.known_disks.insert(dev.clone());
            let path = format!("/dev/{dev}");
            info!("discovered block volume device: {}", path);
            Some(path)
        } else {
            None
        }
    }

    /// Adapt the OCI spec for the VM environment.
    ///
    /// The host-generated config.json contains references to host-side
    /// resources (CNI network namespaces, cgroup paths, etc.) that don't
    /// exist inside the VM. We strip or modify these so crun can create
    /// the container using the VM's own namespaces.
    fn adapt_oci_spec_for_vm(
        &mut self,
        bundle: &std::path::Path,
        volumes: &[VolumeInfo],
    ) -> Result<()> {
        let config_path = bundle.join("config.json");
        let data = std::fs::read_to_string(&config_path)
            .with_context(|| format!("failed to read {}", config_path.display()))?;
        let mut spec: serde_json::Value =
            serde_json::from_str(&data).context("failed to parse config.json")?;

        // Adapt namespaces for VM isolation. The VM provides network, UTS,
        // and IPC isolation. We keep mount and PID namespaces so each
        // container gets its own filesystem view and process tree —
        // essential for multi-container-per-VM (pod) support.
        if let Some(linux) = spec.pointer_mut("/linux") {
            if let Some(obj) = linux.as_object_mut() {
                obj.insert(
                    "namespaces".to_string(),
                    serde_json::json!([
                        {"type": "mount"},
                        {"type": "pid"}
                    ]),
                );
                obj.remove("cgroupsPath");
                obj.remove("maskedPaths");
                obj.remove("readonlyPaths");
                obj.remove("resources");
                obj.remove("seccomp");
            }
        }

        // Replace host mounts with essential system mounts + injected volumes
        let mut mounts = vec![
            serde_json::json!({"destination": "/proc", "type": "proc", "source": "proc"}),
            serde_json::json!({"destination": "/dev", "type": "tmpfs", "source": "tmpfs",
                "options": ["nosuid", "strictatime", "mode=755", "size=65536k"]}),
            serde_json::json!({"destination": "/dev/pts", "type": "devpts", "source": "devpts",
                "options": ["nosuid", "noexec", "newinstance", "ptmxmode=0666", "mode=0620"]}),
            serde_json::json!({"destination": "/sys", "type": "sysfs", "source": "sysfs",
                "options": ["nosuid", "noexec", "nodev", "ro"]}),
        ];

        // Add volume mounts from the shim (CSI, ConfigMap, Secret, emptyDir)
        for vol in volumes {
            if vol.is_block {
                // Block volumes: discover the hot-plugged device by scanning
                // /sys/block for new vdX devices not already known.
                let dev_path = match self.discover_block_device() {
                    Some(p) => p,
                    None => {
                        warn!(
                            "block volume {} not found, skipping mount to {}",
                            vol.source, vol.destination
                        );
                        continue;
                    }
                };
                let mut opts = Vec::new();
                if vol.readonly {
                    opts.push("ro".to_string());
                }
                mounts.push(serde_json::json!({
                    "destination": vol.destination,
                    "type": vol.fs_type,
                    "source": dev_path,
                    "options": opts,
                }));
                info!(
                    "block volume mount: {} -> {} (dev={}, fs={})",
                    vol.source, vol.destination, dev_path, vol.fs_type
                );
            } else {
                // Filesystem volumes: bind-mount from the virtio-fs shared dir
                let mut opts = vec!["rbind".to_string()];
                if vol.readonly {
                    opts.push("ro".to_string());
                }
                mounts.push(serde_json::json!({
                    "destination": vol.destination,
                    "type": "bind",
                    "source": vol.source,
                    "options": opts,
                }));
                info!(
                    "fs volume mount: {} -> {} (ro={})",
                    vol.source, vol.destination, vol.readonly
                );
            }
        }

        if let Some(obj) = spec.as_object_mut() {
            obj.remove("hostname");
            obj.insert("mounts".to_string(), serde_json::Value::Array(mounts));
        }
        if let Some(process) = spec.pointer_mut("/process") {
            if let Some(obj) = process.as_object_mut() {
                obj.remove("apparmorProfile");
                obj.remove("selinuxLabel");
                obj.remove("oomScoreAdj");
            }
        }

        // Write the modified spec back
        let modified = serde_json::to_string_pretty(&spec)?;
        std::fs::write(&config_path, modified)
            .with_context(|| format!("failed to write {}", config_path.display()))?;

        Ok(())
    }

    /// Create a new container from a hot-plugged block device.
    ///
    /// The host shim creates an ext4 disk image containing the OCI bundle
    /// (config.json + rootfs) and hot-plugs it into the VM. The agent
    /// discovers the new block device, mounts it, adapts the OCI spec for
    /// the VM environment, and prepares it for crun. Execution happens in
    /// `start()`.
    pub async fn create(
        &mut self,
        container_id: &str,
        bundle_path: &str,
        stdout_path: Option<&str>,
        stderr_path: Option<&str>,
        volumes: &[VolumeInfo],
    ) -> Result<u32> {
        info!("creating container: id={}", container_id);

        // The bundle_path from the shim tells us the guest mount point.
        // The host has already hot-plugged a block device containing the
        // rootfs + config.json. We need to find and mount it.
        let _bundle = PathBuf::from(bundle_path);

        // Wait for the block device to appear and mount it.
        // The host hot-plugged it just before this RPC — give the kernel
        // time to detect it via ACPI.
        let mount_point = PathBuf::from("/run/containers").join(container_id);
        std::fs::create_dir_all(&mount_point)?;

        let disk_path = self
            .discover_and_mount_new_disk(&mount_point)
            .await
            .context("discover/mount container disk")?;
        info!(
            "mounted container disk {} at {}",
            disk_path,
            mount_point.display()
        );

        // The disk image contains config.json and rootfs/ at its root
        let local_bundle = mount_point.clone();

        // Adapt the OCI spec for the VM environment
        self.adapt_oci_spec_for_vm(&local_bundle, volumes)?;

        let pid = std::process::id();
        self.containers.insert(
            container_id.to_string(),
            Container {
                pid: Some(pid),
                exit_code: None,
                state: ContainerState::Created,
                stdout_path: stdout_path.map(String::from),
                stderr_path: stderr_path.map(String::from),
            },
        );
        info!("container created: id={}", container_id);
        Ok(pid)
    }

    /// Start a previously created container using "crun run".
    pub async fn start(&mut self, container_id: &str) -> Result<u32> {
        info!("starting container: {}", container_id);

        let container = self
            .containers
            .get(container_id)
            .ok_or_else(|| anyhow::anyhow!("container not found: {}", container_id))?;

        // Open stdout/stderr files for crun output (shared dir → host via virtio-fs).
        // The shim tails these files and forwards to containerd FIFOs (kubectl logs).
        let stdout_file = match &container.stdout_path {
            Some(path) => Stdio::from(
                std::fs::File::create(path)
                    .with_context(|| format!("create stdout file: {path}"))?,
            ),
            None => Stdio::null(),
        };
        let stderr_file = match &container.stderr_path {
            Some(path) => Stdio::from(
                std::fs::File::create(path)
                    .with_context(|| format!("create stderr file: {path}"))?,
            ),
            None => Stdio::null(),
        };

        // Bundle path matches the mount point created in create().
        let bundle = PathBuf::from("/run/containers").join(container_id);

        let child = Command::new("/bin/crun")
            .arg("run")
            .arg("--bundle")
            .arg(&bundle)
            .arg(container_id)
            .stdin(Stdio::null())
            .stdout(stdout_file)
            .stderr(stderr_file)
            .spawn()
            .with_context(|| format!("spawn crun run (bundle={})", bundle.display()))?;

        let pid = child.id().unwrap_or(0);

        if let Some(c) = self.containers.get_mut(container_id) {
            c.state = ContainerState::Running;
            c.pid = Some(pid);
        }
        info!("container started: id={}, pid={}", container_id, pid);

        // Create a watch channel for exit notification.
        // Insert the receiver AFTER the spawn succeeds so we never orphan it.
        let (tx, rx) = watch::channel::<Option<ExitStatus>>(None);

        // Wait for exit in background and send status through the watch channel
        let container_id_owned = container_id.to_string();
        tokio::spawn(async move {
            match child.wait_with_output().await {
                Ok(output) => {
                    let code = output.status.code().unwrap_or(137);
                    info!(
                        "container exited: id={} status={}",
                        container_id_owned, output.status,
                    );
                    let _ = tx.send(Some(ExitStatus {
                        code,
                        exited_at: chrono::Utc::now().to_rfc3339(),
                    }));
                }
                Err(e) => {
                    error!("container wait error: id={} err={}", container_id_owned, e);
                    let _ = tx.send(Some(ExitStatus {
                        code: 137,
                        exited_at: chrono::Utc::now().to_rfc3339(),
                    }));
                }
            }
        });

        self.exit_receivers.insert(container_id.to_string(), rx);

        Ok(pid)
    }

    /// Send a signal to a container.
    pub async fn kill(&self, container_id: &str, signal: u32) -> Result<()> {
        info!("killing container: {} signal={}", container_id, signal);

        // Check if container has already exited
        if let Some(container) = self.containers.get(container_id) {
            if container.state == ContainerState::Stopped {
                info!("container {} already stopped, ignoring kill", container_id);
                return Ok(());
            }
        }

        let output = Command::new("/bin/crun")
            .arg("kill")
            .arg(container_id)
            .arg(signal.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("failed to execute crun kill")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // "No such file" means the container already exited — not an error
            if stderr.contains("No such file") || stderr.contains("not found") {
                info!(
                    "container {} already exited (crun kill: {})",
                    container_id,
                    stderr.trim()
                );
                return Ok(());
            }
            anyhow::bail!("crun kill failed: {}", stderr);
        }

        Ok(())
    }

    /// Delete a stopped container.
    pub async fn delete(&mut self, container_id: &str) -> Result<(u32, i32)> {
        info!("deleting container: {}", container_id);

        let output = Command::new("/bin/crun")
            .arg("delete")
            .arg("--force")
            .arg(container_id)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("failed to execute crun delete")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!("crun delete failed (may be ok): {}", stderr);
        }

        let (pid, exit_code) = if let Some(container) = self.containers.remove(container_id) {
            (container.pid.unwrap_or(0), container.exit_code.unwrap_or(0))
        } else {
            (0, 0)
        };

        info!(
            "container deleted: id={}, pid={}, exit_code={}",
            container_id, pid, exit_code
        );
        Ok((pid, exit_code))
    }

    /// Get a clone of the exit receiver for a container.
    pub fn get_exit_receiver(&self, id: &str) -> Option<watch::Receiver<Option<ExitStatus>>> {
        self.exit_receivers.get(id).cloned()
    }

    /// Mark a container as stopped with the given exit code.
    pub fn mark_stopped(&mut self, id: &str, code: i32) {
        if let Some(c) = self.containers.get_mut(id) {
            c.state = ContainerState::Stopped;
            c.exit_code = Some(code);
        }
    }

    /// Get the state of a container as a proto response.
    pub async fn state(
        &self,
        container_id: &str,
    ) -> Result<crate::proto::agent::StateContainerResponse> {
        use crate::proto::agent::{ContainerState as ProtoState, StateContainerResponse};
        use ::protobuf::EnumOrUnknown;

        let mut resp = StateContainerResponse::new();
        resp.container_id = container_id.to_string();

        if let Some(container) = self.containers.get(container_id) {
            resp.pid = container.pid.unwrap_or(0);
            resp.status = match container.state {
                ContainerState::Created => EnumOrUnknown::new(ProtoState::CREATED),
                ContainerState::Running => EnumOrUnknown::new(ProtoState::RUNNING),
                ContainerState::Stopped => EnumOrUnknown::new(ProtoState::STOPPED),
            };
            resp.exit_status = container.exit_code.unwrap_or(0) as u32;
        } else {
            resp.status = EnumOrUnknown::new(ProtoState::UNKNOWN);
        }

        Ok(resp)
    }
}
