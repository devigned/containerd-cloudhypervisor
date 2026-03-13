//! VM lifecycle integration tests.
//!
//! These tests boot real Cloud Hypervisor VMs with a minimal guest kernel
//! and rootfs containing the cloudhv-agent.

use std::time::Duration;

use crate::helpers::TestFixtures;
use crate::skip_if_missing;

/// Test that we can boot a Cloud Hypervisor VM and it starts successfully.
///
/// This test:
/// 1. Creates a VmManager with test config
/// 2. Prepares the state directory
/// 3. Starts the Cloud Hypervisor VMM
/// 4. Creates and boots the VM
/// 5. Waits for the guest agent to become reachable
/// 6. Shuts down and cleans up
#[test]
fn test_vm_boot_and_agent_health() {
    let fixtures = TestFixtures::resolve().expect("failed to resolve test fixtures");
    skip_if_missing!(fixtures);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let config = fixtures.runtime_config();
        let vm_id = format!("test-vm-{}", std::process::id());

        eprintln!("=== Creating VM: {} ===", vm_id);
        let mut vm = containerd_shim_cloudhv::vm::VmManager::new(vm_id.clone(), config)
            .expect("failed to create VmManager");

        // Prepare state directory (needs root for /run/cloudhv/)
        vm.prepare().await.expect("VM prepare failed");
        assert!(vm.state_dir().exists(), "state dir should exist");
        assert!(vm.shared_dir().exists(), "shared dir should exist");

        // Start Cloud Hypervisor
        eprintln!("=== Starting Cloud Hypervisor VMM ===");
        vm.spawn_vmm_in_netns(None).expect("failed to spawn VMM");
        vm.wait_vmm_ready().await.expect("failed to start CH VMM");

        // Create and boot VM
        eprintln!("=== Creating and booting VM ===");
        vm.create_and_boot_vm(None, None)
            .await
            .expect("failed to create and boot VM");

        // Wait for agent
        eprintln!("=== Waiting for guest agent ===");
        vm.wait_for_agent().await.expect("agent must be reachable");
        eprintln!("=== Guest agent is ready! ===");

        // Shutdown and cleanup
        eprintln!("=== Shutting down VM ===");
        vm.cleanup().await.expect("failed to clean up VM");
        assert!(
            !vm.state_dir().exists(),
            "state dir should be removed after cleanup"
        );

        eprintln!("=== Test passed: VM lifecycle complete ===");
    });
}

/// Test that VmManager correctly sets up the state directory structure.
#[test]
fn test_vm_state_directory_setup() {
    let fixtures = TestFixtures::resolve().expect("failed to resolve test fixtures");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let config = fixtures.runtime_config();
        let vm_id = format!("test-state-{}", std::process::id());

        let vm = containerd_shim_cloudhv::vm::VmManager::new(vm_id.clone(), config)
            .expect("failed to create VmManager");

        // CID should be >= 3
        assert!(vm.cid() >= 3, "CID should be >= 3, got {}", vm.cid());

        // State dir should be under /run/cloudhv/<vm_id>
        assert!(
            vm.state_dir().to_string_lossy().contains(&vm_id),
            "state dir should contain vm_id"
        );

        // Prepare should create directories
        // This may fail if /run/cloudhv is not writable (needs root)
        match vm.prepare().await {
            Ok(()) => {
                assert!(vm.state_dir().exists());
                assert!(vm.shared_dir().exists());
                // Clean up
                let _ = tokio::fs::remove_dir_all(vm.state_dir()).await;
            }
            Err(e) => {
                eprintln!(
                    "SKIPPING directory creation check (likely needs root): {}",
                    e
                );
            }
        }
    });
}

/// Test that multiple VMs get unique CIDs.
#[test]
fn test_unique_cid_allocation() {
    let fixtures = TestFixtures::resolve().expect("failed to resolve test fixtures");
    let config = fixtures.runtime_config();

    let vm1 =
        containerd_shim_cloudhv::vm::VmManager::new("cid-test-1".into(), config.clone()).unwrap();
    let vm2 =
        containerd_shim_cloudhv::vm::VmManager::new("cid-test-2".into(), config.clone()).unwrap();
    let vm3 = containerd_shim_cloudhv::vm::VmManager::new("cid-test-3".into(), config).unwrap();

    assert_ne!(vm1.cid(), vm2.cid(), "VMs should get unique CIDs");
    assert_ne!(vm2.cid(), vm3.cid(), "VMs should get unique CIDs");
    assert_ne!(vm1.cid(), vm3.cid(), "VMs should get unique CIDs");
    assert!(vm1.cid() >= 3, "CIDs should start at 3+");
    assert!(vm2.cid() >= 3, "CIDs should start at 3+");
    assert!(vm3.cid() >= 3, "CIDs should start at 3+");

    eprintln!(
        "CIDs allocated: {}, {}, {}",
        vm1.cid(),
        vm2.cid(),
        vm3.cid()
    );
}

/// Test Cloud Hypervisor VM config JSON generation.
#[test]
fn test_vm_config_json_generation() {
    use cloudhv_common::types::*;

    let config = VmConfig {
        payload: VmPayload {
            kernel: "/opt/vmlinux".to_string(),
            cmdline: Some("console=hvc0 root=/dev/vda rw".to_string()),
            initramfs: None,
        },
        cpus: VmCpus {
            boot_vcpus: 1,
            max_vcpus: 2,
        },
        memory: VmMemory {
            size: 128 * 1024 * 1024,
            shared: true,
            hotplug_size: None,
            hotplug_method: None,
        },
        disks: vec![VmDisk {
            path: "/opt/rootfs.ext4".to_string(),
            readonly: false,
            id: None,
        }],
        net: vec![],
        fs: vec![],
        vsock: Some(VmVsock {
            cid: 3,
            socket: "/run/vsock.sock".to_string(),
        }),
        serial: Some(VmConsoleConfig::off()),
        console: Some(VmConsoleConfig::off()),
        balloon: None,
        tpm: None,
    };

    let json = serde_json::to_string_pretty(&config).expect("failed to serialize VM config");
    eprintln!("VM Config JSON:\n{}", json);

    // Verify key fields
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        parsed["payload"]["kernel"].as_str().unwrap(),
        "/opt/vmlinux"
    );
    assert_eq!(parsed["cpus"]["boot_vcpus"].as_u64().unwrap(), 1);
    assert_eq!(
        parsed["memory"]["size"].as_u64().unwrap(),
        128 * 1024 * 1024
    );
    assert!(parsed["memory"]["shared"].as_bool().unwrap());
    assert_eq!(parsed["vsock"]["cid"].as_u64().unwrap(), 3);
    assert_eq!(parsed["serial"]["mode"].as_str().unwrap(), "Off");
}

/// Test ttrpc health check RPC against the guest agent.
///
/// This verifies the full ttrpc path: shim -> vsock CONNECT -> ttrpc client
/// -> agent ttrpc server -> HealthService.Check -> response.
#[test]
fn test_ttrpc_health_check_rpc() {
    let fixtures = TestFixtures::resolve().expect("failed to resolve test fixtures");
    skip_if_missing!(fixtures);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let config = fixtures.runtime_config();
        let vm_id = format!("test-health-{}", std::process::id());

        eprintln!("=== Creating VM for health check test: {} ===", vm_id);
        let mut vm = containerd_shim_cloudhv::vm::VmManager::new(vm_id.clone(), config)
            .expect("failed to create VmManager");

        vm.prepare().await.expect("VM prepare failed");
        vm.spawn_vmm_in_netns(None).expect("VMM spawn failed");
        vm.wait_vmm_ready().await.expect("VMM ready failed");
        vm.create_and_boot_vm(None, None)
            .await
            .expect("boot failed");

        tokio::time::timeout(Duration::from_secs(30), vm.wait_for_agent())
            .await
            .expect("agent wait timed out (30s)")
            .expect("agent must be reachable");
        eprintln!("=== Agent ready ===");

        // Connect ttrpc and verify health check RPC
        let vsock_client = containerd_shim_cloudhv::vsock::VsockClient::new(vm.vsock_socket());
        let (_agent, health) =
            tokio::time::timeout(Duration::from_secs(5), vsock_client.connect_ttrpc())
                .await
                .expect("ttrpc connect timed out")
                .expect("ttrpc connect failed");

        let ctx = ttrpc::context::with_duration(Duration::from_secs(5));
        let resp = health
            .check(ctx, &cloudhv_proto::CheckRequest::new())
            .await
            .expect("health check RPC failed");
        assert!(resp.ready, "agent must report ready=true");
        eprintln!("=== ttrpc health check: ready={} ===", resp.ready);

        drop(_agent);
        drop(health);

        eprintln!("=== Cleaning up ===");
        vm.cleanup().await.expect("cleanup failed");
        eprintln!("=== ttrpc health check test complete ===");
    });
}

/// Test I/O directory setup for container output.
#[test]
fn test_io_directory_creation() {
    let fixtures = TestFixtures::resolve().expect("failed to resolve test fixtures");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let config = fixtures.runtime_config();
        let vm_id = format!("test-io-{}", std::process::id());

        let vm = containerd_shim_cloudhv::vm::VmManager::new(vm_id.clone(), config)
            .expect("failed to create VmManager");

        match vm.prepare().await {
            Ok(()) => {
                // Verify shared dir exists for I/O
                assert!(vm.shared_dir().exists(), "shared dir should exist");

                // Create I/O dir like the shim would
                let io_dir = vm.shared_dir().join("io").join("test-container");
                std::fs::create_dir_all(&io_dir).expect("failed to create io dir");
                let stdout_path = io_dir.join("stdout");
                std::fs::write(&stdout_path, "").expect("failed to create stdout file");
                assert!(stdout_path.exists(), "stdout file should exist");

                // Cleanup
                let _ = tokio::fs::remove_dir_all(vm.state_dir()).await;
                eprintln!("=== I/O directory test passed ===");
            }
            Err(e) => {
                eprintln!("SKIP: directory creation needs root: {e}");
            }
        }
    });
}

/// Test VM config with hotplug memory settings.
#[test]
fn test_vm_config_with_hotplug() {
    use cloudhv_common::types::*;

    let config = VmConfig {
        payload: VmPayload {
            kernel: "/opt/vmlinux".to_string(),
            cmdline: Some("console=hvc0".to_string()),
            initramfs: None,
        },
        cpus: VmCpus {
            boot_vcpus: 1,
            max_vcpus: 4,
        },
        memory: VmMemory {
            size: 128 * 1024 * 1024,
            shared: true,
            hotplug_size: Some(512 * 1024 * 1024),
            hotplug_method: Some("VirtioMem".to_string()),
        },
        disks: vec![],
        net: vec![],
        fs: vec![],
        vsock: None,
        serial: None,
        console: None,
        balloon: None,
        tpm: None,
    };

    let json = serde_json::to_string_pretty(&config).expect("failed to serialize");
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed["cpus"]["max_vcpus"].as_u64().unwrap(), 4);
    assert_eq!(
        parsed["memory"]["hotplug_size"].as_u64().unwrap(),
        512 * 1024 * 1024
    );
    assert_eq!(
        parsed["memory"]["hotplug_method"].as_str().unwrap(),
        "VirtioMem"
    );
    eprintln!("VM config with hotplug:\n{json}");
    eprintln!("=== Hotplug config test passed ===");
}

// ====================================================================
// Phase 3 integration tests: VM pool, hotplug resize, timed lifecycle
// ====================================================================

/// Benchmark: measure end-to-end VM lifecycle overhead with timing breakdown.
///
/// Reports time for each phase:
///   1. VmManager::new + prepare (shim overhead)
///   2. spawn_vmm_in_netns (Cloud Hypervisor process startup)
///   3. create_and_boot_vm (CH API create + boot)
///   4. wait_for_agent (guest boot + agent startup)
///   5. vsock ttrpc connect (ttrpc handshake)
///   6. cleanup (shutdown + remove state)
///
/// This is an integration test that acts as a benchmark — run with --nocapture
/// to see timing output.
#[test]
fn test_vm_lifecycle_timing_breakdown() {
    let fixtures = TestFixtures::resolve().expect("failed to resolve test fixtures");
    skip_if_missing!(fixtures);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let config = fixtures.runtime_config();
        let vm_id = format!("bench-lifecycle-{}", std::process::id());

        eprintln!("\n=== VM Lifecycle Timing Breakdown ===");
        let total_start = std::time::Instant::now();

        // Phase 1: VmManager creation + prepare
        let t0 = std::time::Instant::now();
        let mut vm = containerd_shim_cloudhv::vm::VmManager::new(vm_id.clone(), config.clone())
            .expect("VmManager::new failed");
        vm.prepare().await.expect("VM prepare failed");
        let shim_overhead = t0.elapsed();
        eprintln!("  [1] Shim setup (new + prepare):  {:>8.1?}", shim_overhead);

        // Phase 2: Cloud Hypervisor VMM
        let t2 = std::time::Instant::now();
        vm.spawn_vmm_in_netns(None).expect("VMM spawn failed");
        vm.wait_vmm_ready().await.expect("VMM ready failed");
        let vmm_time = t2.elapsed();
        eprintln!("  [2] Cloud Hypervisor startup:    {:>8.1?}", vmm_time);

        // Phase 3: VM create + boot
        let t3 = std::time::Instant::now();
        vm.create_and_boot_vm(None, None)
            .await
            .expect("boot failed");
        let boot_time = t3.elapsed();
        eprintln!("  [3] VM create + boot (CH API):   {:>8.1?}", boot_time);

        // Phase 4: Wait for agent
        let t4 = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(30), vm.wait_for_agent())
            .await
            .expect("agent wait timed out (30s)")
            .expect("agent must be reachable");
        let agent_time = t4.elapsed();
        eprintln!("  [4] Guest boot + agent ready:    {:>8.1?}", agent_time);

        // Phase 5: ttrpc connect
        let t5 = std::time::Instant::now();
        let vsock_client = containerd_shim_cloudhv::vsock::VsockClient::new(vm.vsock_socket());
        let _ttrpc_result =
            tokio::time::timeout(Duration::from_secs(5), vsock_client.connect_ttrpc())
                .await
                .expect("ttrpc connect timed out (5s)")
                .expect("ttrpc connect failed");
        let ttrpc_time = t5.elapsed();
        eprintln!("  [5] ttrpc connect:               {:>8.1?}", ttrpc_time);

        // Phase 6: cleanup
        let t6 = std::time::Instant::now();
        vm.cleanup().await.expect("cleanup failed");
        let cleanup_time = t6.elapsed();
        eprintln!("  [6] Shutdown + cleanup:          {:>8.1?}", cleanup_time);

        let total = total_start.elapsed();
        let overhead = shim_overhead + vmm_time + cleanup_time;
        let guest = boot_time + agent_time;
        eprintln!("  ─────────────────────────────────────────");
        eprintln!("  Total:                           {:>8.1?}", total);
        eprintln!(
            "  Shim/host overhead:              {:>8.1?} ({:.0}%)",
            overhead,
            overhead.as_secs_f64() / total.as_secs_f64() * 100.0
        );
        eprintln!(
            "  Guest (boot + agent):            {:>8.1?} ({:.0}%)",
            guest,
            guest.as_secs_f64() / total.as_secs_f64() * 100.0
        );
        eprintln!("=== Timing breakdown complete ===\n");
    });
}

/// Test VM resize API call format.
#[test]
fn test_vm_resize_api() {
    let fixtures = TestFixtures::resolve().expect("failed to resolve test fixtures");
    skip_if_missing!(fixtures);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let mut config = fixtures.runtime_config();
        config.hotplug_memory_mb = 256; // Enable hotplug

        let vm_id = format!("test-resize-{}", std::process::id());
        let mut vm = containerd_shim_cloudhv::vm::VmManager::new(vm_id, config)
            .expect("VmManager::new failed");

        vm.prepare().await.expect("VM prepare failed");

        vm.spawn_vmm_in_netns(None).expect("VMM spawn failed");
        vm.wait_vmm_ready().await.expect("VMM ready failed");
        vm.create_and_boot_vm(None, None)
            .await
            .expect("boot failed");

        // Try resize — may fail if hotplug not fully supported by kernel
        eprintln!("=== Testing VM resize ===");
        match vm.resize(Some(2), None).await {
            Ok(()) => eprintln!("  Resize to 2 vCPUs: OK"),
            Err(e) => eprintln!("  Resize to 2 vCPUs: {e} (may need kernel support)"),
        }

        vm.cleanup().await.expect("cleanup failed");
        eprintln!("=== VM resize test complete ===");
    });
}

/// Test that container stdout and stderr are captured.
///
/// This test:
/// 1. Boots a VM using VmManager directly
/// 2. Creates a disk image with a shell script that writes to stdout and stderr
/// 3. Hot-plugs the disk, creates and starts the container
/// 4. Waits for the container to exit and asserts exit code 0
/// 5. Reads the stdout/stderr files on the host and asserts expected content
#[test]
fn test_container_logs_captured() {
    let fixtures = TestFixtures::resolve().expect("failed to resolve test fixtures");
    skip_if_missing!(fixtures);

    // We need /bin/busybox or /bin/sh on the host to copy into the rootfs
    let shell_src = if std::path::Path::new("/bin/busybox").exists() {
        std::path::PathBuf::from("/bin/busybox")
    } else if std::path::Path::new("/bin/sh").exists() {
        std::path::PathBuf::from("/bin/sh")
    } else {
        eprintln!("SKIPPING TEST: neither /bin/busybox nor /bin/sh found");
        return;
    };

    for tool in ["mkfs.ext4", "mount", "umount"] {
        if std::process::Command::new("which")
            .arg(tool)
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            eprintln!("SKIPPING TEST: {tool} not found in PATH");
            return;
        }
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let config = fixtures.runtime_config();
        let vm_id = format!("logs-test-{}", std::process::id());
        let container_id = format!("logs-ctr-{}", std::process::id());

        eprintln!(
            "\n=== test_container_logs_captured: booting VM {} ===",
            vm_id
        );

        // ── Boot VM ───────────────────────────────────────────────────
        let mut vm = containerd_shim_cloudhv::vm::VmManager::new(vm_id.clone(), config)
            .expect("failed to create VmManager");
        vm.prepare().await.expect("prepare");
        vm.spawn_vmm_in_netns(None).expect("vmm");
        vm.wait_vmm_ready().await.expect("vmm ready");
        vm.create_and_boot_vm(None, None).await.expect("boot");
        vm.wait_for_agent().await.expect("agent");

        // ── Connect ttrpc agent ───────────────────────────────────────
        let vsock_client = containerd_shim_cloudhv::vsock::VsockClient::new(vm.vsock_socket());
        let (agent, _health) = vsock_client.connect_ttrpc().await.expect("ttrpc");

        // ── Create disk image with a script that writes to both fds ───
        let disk_path = create_script_disk_image(
            vm.state_dir(),
            &shell_src,
            &container_id,
            &["sh", "-c", "echo 'HELLO_STDOUT' && echo 'HELLO_STDERR' >&2"],
        );

        // ── Hot-plug disk ─────────────────────────────────────────────
        vm.add_disk(&disk_path.to_string_lossy(), &container_id, false)
            .await
            .expect("add_disk");

        // ── CreateContainer ───────────────────────────────────────────
        let mut create_req = cloudhv_proto::CreateContainerRequest::new();
        create_req.container_id = container_id.clone();
        create_req.bundle_path = format!("/run/containers/{}", container_id);
        let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(30));
        agent
            .create_container(ctx, &create_req)
            .await
            .expect("CreateContainer");

        // ── StartContainer ────────────────────────────────────────────
        let mut start_req = cloudhv_proto::StartContainerRequest::new();
        start_req.container_id = container_id.clone();
        let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(30));
        agent
            .start_container(ctx, &start_req)
            .await
            .expect("StartContainer");

        // ── WaitContainer ─────────────────────────────────────────────
        let mut wait_req = cloudhv_proto::WaitContainerRequest::new();
        wait_req.container_id = container_id.clone();
        let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(60));
        let wait_resp = agent
            .wait_container(ctx, &wait_req)
            .await
            .expect("WaitContainer");
        assert_eq!(
            wait_resp.exit_status, 0,
            "container should exit with status 0, got {}",
            wait_resp.exit_status
        );

        // ── Assert logs captured via GetContainerLogs RPC ──────────────
        let mut log_req = cloudhv_proto::GetContainerLogsRequest::new();
        log_req.container_id = container_id.clone();
        log_req.offset = 0;
        let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(5));
        let log_resp = agent
            .get_container_logs(ctx, &log_req)
            .await
            .expect("GetContainerLogs");

        let stdout_content = String::from_utf8_lossy(&log_resp.stdout);
        assert!(
            stdout_content.contains("HELLO_STDOUT"),
            "stdout should contain HELLO_STDOUT, got: {stdout_content:?}"
        );
        eprintln!("  stdout OK: {:?}", stdout_content.trim());

        let stderr_content = String::from_utf8_lossy(&log_resp.stderr);
        assert!(
            stderr_content.contains("HELLO_STDERR"),
            "stderr should contain HELLO_STDERR, got: {stderr_content:?}"
        );
        eprintln!("  stderr OK: {:?}", stderr_content.trim());

        // ── Cleanup ───────────────────────────────────────────────────
        let mut del_req = cloudhv_proto::DeleteContainerRequest::new();
        del_req.container_id = container_id.clone();
        let ctx = ttrpc::context::with_duration(std::time::Duration::from_secs(10));
        let _ = agent.delete_container(ctx, &del_req).await;

        drop(agent);
        vm.cleanup().await.expect("cleanup failed");
        let _ = std::fs::remove_file(&disk_path);

        eprintln!("=== test_container_logs_captured: PASSED ===\n");
    });
}

/// Create an ext4 disk image containing an OCI bundle that runs a shell command.
///
/// Similar to [`create_echo_disk_image`] but takes arbitrary args and uses
/// busybox/sh instead of a dedicated binary. The disk is 32 MB.
fn create_script_disk_image(
    state_dir: &std::path::Path,
    shell_src: &std::path::Path,
    name: &str,
    args: &[&str],
) -> std::path::PathBuf {
    let disk_path = state_dir.join(format!("{name}.img"));
    let bundle_tmp = state_dir.join(format!("{name}-bundle"));
    let rootfs_tmp = bundle_tmp.join("rootfs");
    let bin_dir = rootfs_tmp.join("bin");
    std::fs::create_dir_all(&bin_dir).expect("mkdir rootfs/bin");

    // Copy the shell binary into rootfs/bin/sh
    std::fs::copy(shell_src, bin_dir.join("sh")).expect("cp shell");
    std::process::Command::new("chmod")
        .args(["755", &bin_dir.join("sh").to_string_lossy()])
        .status()
        .expect("chmod");

    // If the source is busybox, create symlinks for common applets
    if shell_src.to_string_lossy().contains("busybox") {
        for applet in &["echo", "cat", "ls", "sleep"] {
            let _ = std::os::unix::fs::symlink("sh", bin_dir.join(applet));
        }
    }

    // Write OCI config.json
    let oci_args: Vec<serde_json::Value> = args
        .iter()
        .map(|a| serde_json::Value::String(a.to_string()))
        .collect();
    let oci_config = serde_json::json!({
        "ociVersion": "1.0.2",
        "process": {
            "terminal": false,
            "user": { "uid": 0, "gid": 0 },
            "args": oci_args,
            "env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
            "cwd": "/"
        },
        "root": { "path": "rootfs", "readonly": false },
        "linux": { "namespaces": [{"type": "pid"}, {"type": "mount"}] }
    });
    std::fs::write(
        bundle_tmp.join("config.json"),
        serde_json::to_string_pretty(&oci_config).unwrap(),
    )
    .expect("write config.json");

    // Create 32 MB ext4 disk image
    let f = std::fs::File::create(&disk_path).expect("create disk");
    f.set_len(32 * 1024 * 1024).expect("set_len");
    drop(f);
    assert!(run_cmd_status(
        "mkfs.ext4",
        &["-q", "-F", &disk_path.to_string_lossy()]
    ));
    let mount_dir = disk_path.with_extension("mnt");
    std::fs::create_dir_all(&mount_dir).expect("mkdir mount");
    assert!(run_cmd_status(
        "mount",
        &[
            "-o",
            "loop",
            &disk_path.to_string_lossy(),
            &mount_dir.to_string_lossy()
        ]
    ));
    let img_rootfs = mount_dir.join("rootfs");
    std::fs::create_dir_all(&img_rootfs).expect("mkdir img rootfs");
    assert!(run_cmd_status(
        "cp",
        &[
            "-a",
            "--",
            &format!("{}/.", rootfs_tmp.display()),
            &img_rootfs.to_string_lossy()
        ]
    ));
    std::fs::copy(
        bundle_tmp.join("config.json"),
        mount_dir.join("config.json"),
    )
    .expect("cp config.json");
    assert!(run_cmd_status("umount", &[&mount_dir.to_string_lossy()]));
    std::fs::remove_dir(&mount_dir).ok();
    std::fs::remove_dir_all(&bundle_tmp).ok();

    disk_path
}

/// End-to-end test: boot a VM with networking, start an HTTP echo container,
/// and verify that an HTTP request from the host reaches the container and
/// gets a valid response.
///
/// This proves the entire networking stack works:
///   veth ↔ TC redirect ↔ TAP ↔ virtio-net ↔ guest eth0 (kernel IP_PNP)
///
/// Requires:
/// - Root (for netns, TAP, TC, mount)
/// - KVM or MSHV
/// - cloud-hypervisor, kernel, rootfs
/// - A static HTTP echo binary: set `CLOUDHV_TEST_HTTP_ECHO` env var
///   (e.g. hashicorp/http-echo or any binary that listens on a port)
///
/// Download http-echo for testing:
///   curl -Lo /usr/local/bin/http-echo \
///     https://github.com/hashicorp/http-echo/releases/download/v0.2.3/http-echo_0.2.3_linux_amd64
///   chmod +x /usr/local/bin/http-echo
#[test]
fn test_echo_container_with_networking() {
    let fixtures = TestFixtures::resolve().expect("failed to resolve test fixtures");
    skip_if_missing!(fixtures);

    // Resolve the http-echo binary
    let http_echo_path = std::env::var("CLOUDHV_TEST_HTTP_ECHO")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/usr/local/bin/http-echo"));
    if !http_echo_path.exists() {
        eprintln!(
            "SKIPPING TEST: http-echo binary not found at {}",
            http_echo_path.display()
        );
        eprintln!("Set CLOUDHV_TEST_HTTP_ECHO or install http-echo to /usr/local/bin/http-echo");
        return;
    }

    // Check that networking tools are available
    for tool in ["nsenter", "ip", "tc", "curl", "mkfs.ext4"] {
        if std::process::Command::new("which")
            .arg(tool)
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            eprintln!("SKIPPING TEST: {tool} not found in PATH");
            return;
        }
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let vm_id = format!("echo-test-{}", std::process::id());
        let netns_name = format!("chtest-{}", std::process::id());
        let netns_path = format!("/var/run/netns/{netns_name}");
        let veth_host = "veth-chtest-h";
        let veth_ns = "veth-chtest-ns";
        let test_ip = "10.200.200.2";
        let host_ip = "10.200.200.1";
        let netmask = "255.255.255.0";
        let echo_port = 5678;
        let echo_text = "hello-from-cloudhv-integration-test";

        // Cleanup guard — ensures netns and temp files are cleaned up on
        // exit, even on panic.
        struct NetnsGuard {
            name: String,
            veth_host: String,
            disk_path: Option<std::path::PathBuf>,
        }
        impl Drop for NetnsGuard {
            fn drop(&mut self) {
                let _ = std::process::Command::new("ip")
                    .args(["link", "delete", &self.veth_host])
                    .status();
                let _ = std::process::Command::new("ip")
                    .args(["netns", "delete", &self.name])
                    .status();
                if let Some(ref p) = self.disk_path {
                    let _ = std::fs::remove_file(p);
                    let _ = std::fs::remove_dir(p.with_extension("mnt"));
                }
            }
        }
        let _guard = NetnsGuard {
            name: netns_name.clone(),
            veth_host: veth_host.to_string(),
            disk_path: None,
        };

        eprintln!("\n╔══════════════════════════════════════════════════════════╗");
        eprintln!("║  Echo Container with Networking — E2E Integration Test  ║");
        eprintln!("╠══════════════════════════════════════════════════════════╣");

        // ── Phase 1: Create test network namespace ────────────────────
        eprintln!("  Phase 1 │ Setting up test network namespace");
        run_cmd("ip", &["netns", "add", &netns_name]);

        // Create veth pair: host end stays in default netns, peer goes into test netns
        run_cmd(
            "ip",
            &[
                "link", "add", veth_host, "type", "veth", "peer", "name", veth_ns,
            ],
        );
        run_cmd("ip", &["link", "set", veth_ns, "netns", &netns_name]);

        // Assign IP to host end and bring it up
        run_cmd(
            "ip",
            &["addr", "add", &format!("{host_ip}/24"), "dev", veth_host],
        );
        run_cmd("ip", &["link", "set", veth_host, "up"]);

        // Assign pod IP to netns end and bring it up
        run_nsenter(
            &netns_path,
            &[
                "ip",
                "addr",
                "add",
                &format!("{test_ip}/24"),
                "dev",
                veth_ns,
            ],
        );
        run_nsenter(&netns_path, &["ip", "link", "set", veth_ns, "up"]);
        run_nsenter(&netns_path, &["ip", "link", "set", "lo", "up"]);

        // Add default route inside netns pointing at host
        run_nsenter(
            &netns_path,
            &["ip", "route", "add", "default", "via", host_ip],
        );

        // Create TAP device inside netns
        let tap_name = format!("tap_{}", &vm_id[..8.min(vm_id.len())]);
        run_nsenter(
            &netns_path,
            &["ip", "tuntap", "add", &tap_name, "mode", "tap"],
        );
        run_nsenter(&netns_path, &["ip", "link", "set", &tap_name, "up"]);

        // Get MAC address of veth-ns (the VM will use this MAC)
        let mac_output = std::process::Command::new("nsenter")
            .args([
                &format!("--net={netns_path}"),
                "--",
                "ip",
                "-j",
                "addr",
                "show",
                "dev",
                veth_ns,
            ])
            .output()
            .expect("get MAC");
        let addrs: serde_json::Value =
            serde_json::from_slice(&mac_output.stdout).unwrap_or(serde_json::json!([]));
        let tap_mac = addrs
            .as_array()
            .and_then(|a| a.first())
            .and_then(|iface| iface.get("address"))
            .and_then(|a| a.as_str())
            .unwrap_or("aa:bb:cc:dd:ee:ff")
            .to_string();

        // TC redirect: veth-ns ↔ TAP (bidirectional L2 forwarding)
        run_nsenter(
            &netns_path,
            &["tc", "qdisc", "add", "dev", veth_ns, "ingress"],
        );
        run_nsenter(
            &netns_path,
            &[
                "tc", "filter", "add", "dev", veth_ns, "parent", "ffff:", "protocol", "all", "u32",
                "match", "u32", "0", "0", "action", "mirred", "egress", "redirect", "dev",
                &tap_name,
            ],
        );
        run_nsenter(
            &netns_path,
            &["tc", "qdisc", "add", "dev", &tap_name, "ingress"],
        );
        run_nsenter(
            &netns_path,
            &[
                "tc", "filter", "add", "dev", &tap_name, "parent", "ffff:", "protocol", "all",
                "u32", "match", "u32", "0", "0", "action", "mirred", "egress", "redirect", "dev",
                veth_ns,
            ],
        );

        // Flush IP from veth-ns so packets traverse TC into VM
        run_nsenter(&netns_path, &["ip", "addr", "flush", "dev", veth_ns]);

        eprintln!("           │ netns={netns_name} tap={tap_name} mac={tap_mac}");
        eprintln!("           │ VM IP={test_ip} host IP={host_ip}");

        // ── Phase 2: Boot VM with networking ──────────────────────────
        eprintln!("  Phase 2 │ Booting VM with networking");
        let mut config = fixtures.runtime_config();
        // Add kernel IP config: ip=<addr>::<gw>:<mask>::eth0:off
        config.kernel_args = format!(
            "{} ip={test_ip}::{host_ip}:{netmask}::eth0:off",
            config.kernel_args
        );

        let mut vm = containerd_shim_cloudhv::vm::VmManager::new(vm_id.clone(), config)
            .expect("VmManager::new");
        vm.prepare().await.expect("prepare");

        vm.spawn_vmm_in_netns(Some(&netns_path))
            .expect("spawn_vmm_in_netns");
        vm.wait_vmm_ready().await.expect("vmm ready");

        vm.create_and_boot_vm(Some(&tap_name), Some(&tap_mac))
            .await
            .expect("create_and_boot_vm");

        tokio::time::timeout(Duration::from_secs(30), vm.wait_for_agent())
            .await
            .expect("agent timeout")
            .expect("agent health");
        eprintln!("           │ VM booted, agent healthy");

        // ── Phase 3: Create disk image with http-echo ─────────────────
        eprintln!("  Phase 3 │ Creating container disk image");
        let disk_path = vm.state_dir().join("echo-container.img");

        // Build an OCI bundle with http-echo
        let bundle_tmp = vm.state_dir().join("echo-bundle");
        let rootfs_tmp = bundle_tmp.join("rootfs");
        std::fs::create_dir_all(&rootfs_tmp).expect("mkdir rootfs");
        std::fs::copy(&http_echo_path, rootfs_tmp.join("http-echo")).expect("cp http-echo");

        // Make it executable
        std::process::Command::new("chmod")
            .args(["755", &rootfs_tmp.join("http-echo").to_string_lossy()])
            .status()
            .expect("chmod");

        // Write OCI config.json
        let oci_config = serde_json::json!({
            "ociVersion": "1.0.2",
            "process": {
                "terminal": false,
                "user": { "uid": 0, "gid": 0 },
                "args": [
                    "/http-echo",
                    &format!("-text={echo_text}"),
                    &format!("-listen=:{echo_port}")
                ],
                "env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
                "cwd": "/"
            },
            "root": { "path": "rootfs", "readonly": false },
            "linux": { "namespaces": [{"type": "pid"}, {"type": "mount"}] }
        });
        std::fs::write(
            bundle_tmp.join("config.json"),
            serde_json::to_string_pretty(&oci_config).unwrap(),
        )
        .expect("write config.json");

        // Create ext4 disk image (64MB, sparse)
        let f = std::fs::File::create(&disk_path).expect("create disk");
        f.set_len(64 * 1024 * 1024).expect("set_len");
        drop(f);

        assert!(
            run_cmd_status("mkfs.ext4", &["-q", "-F", &disk_path.to_string_lossy()]),
            "mkfs.ext4 failed"
        );

        let mount_dir = disk_path.with_extension("mnt");
        std::fs::create_dir_all(&mount_dir).expect("mkdir mount");
        assert!(
            run_cmd_status(
                "mount",
                &[
                    "-o",
                    "loop",
                    &disk_path.to_string_lossy(),
                    &mount_dir.to_string_lossy()
                ]
            ),
            "mount failed"
        );

        // Copy rootfs and config.json into the disk image
        let img_rootfs = mount_dir.join("rootfs");
        std::fs::create_dir_all(&img_rootfs).expect("mkdir img rootfs");
        assert!(
            run_cmd_status(
                "cp",
                &[
                    "-a",
                    "--",
                    &format!("{}/.", rootfs_tmp.display()),
                    &img_rootfs.to_string_lossy(),
                ]
            ),
            "cp rootfs failed"
        );
        std::fs::copy(
            bundle_tmp.join("config.json"),
            mount_dir.join("config.json"),
        )
        .expect("cp config.json");

        assert!(
            run_cmd_status("umount", &[&mount_dir.to_string_lossy()]),
            "umount failed"
        );
        std::fs::remove_dir(&mount_dir).ok();

        eprintln!("           │ disk image: {}", disk_path.display());

        // ── Phase 4: Hot-plug disk and start container ────────────────
        eprintln!("  Phase 4 │ Hot-plugging disk and starting container");
        let container_id = format!("echo-ctr-{}", std::process::id());

        vm.add_disk(&disk_path.to_string_lossy(), &container_id, false)
            .await
            .expect("add_disk");

        // Set up I/O directory for container stdout/stderr
        let io_dir = vm.shared_dir().join("io").join(&container_id);
        std::fs::create_dir_all(&io_dir).expect("mkdir io");

        // Connect to agent via ttrpc
        let vsock_client = containerd_shim_cloudhv::vsock::VsockClient::new(vm.vsock_socket());
        let (agent, _health) =
            tokio::time::timeout(Duration::from_secs(5), vsock_client.connect_ttrpc())
                .await
                .expect("ttrpc connect timeout")
                .expect("ttrpc connect");

        // CreateContainer RPC
        let mut create_req = cloudhv_proto::CreateContainerRequest::new();
        create_req.container_id = container_id.clone();
        create_req.bundle_path = format!("/run/containers/{}", container_id);

        let ctx = ttrpc::context::with_duration(Duration::from_secs(30));
        let create_resp = agent
            .create_container(ctx, &create_req)
            .await
            .expect("CreateContainer RPC failed");
        eprintln!("           │ container created (pid={})", create_resp.pid);

        // StartContainer RPC
        let mut start_req = cloudhv_proto::StartContainerRequest::new();
        start_req.container_id = container_id.clone();
        let ctx = ttrpc::context::with_duration(Duration::from_secs(30));
        agent
            .start_container(ctx, &start_req)
            .await
            .expect("StartContainer RPC failed");
        eprintln!("           │ container started");

        // Give http-echo a moment to bind the port
        tokio::time::sleep(Duration::from_secs(2)).await;

        // ── Phase 5: Curl the echo container from the host ────────────
        eprintln!("  Phase 5 │ Sending HTTP request to VM");
        let url = format!("http://{test_ip}:{echo_port}/");
        eprintln!("           │ curl {url}");

        let curl_output = std::process::Command::new("curl")
            .args(["-s", "--connect-timeout", "5", &url])
            .output()
            .expect("curl command failed");

        let response = String::from_utf8_lossy(&curl_output.stdout)
            .trim()
            .to_string();

        eprintln!("           │ response: \"{response}\"");
        eprintln!(
            "           │ curl exit: {}",
            curl_output.status.code().unwrap_or(-1)
        );

        assert!(
            curl_output.status.success(),
            "curl failed: {}",
            String::from_utf8_lossy(&curl_output.stderr)
        );
        assert!(
            response.contains(echo_text),
            "expected response to contain \"{echo_text}\", got: \"{response}\""
        );

        eprintln!("           │ ✅ Echo response verified!");

        // ── Phase 6: Cleanup ──────────────────────────────────────────
        eprintln!("  Phase 6 │ Cleaning up");

        // Delete container via agent
        let mut del_req = cloudhv_proto::DeleteContainerRequest::new();
        del_req.container_id = container_id.clone();
        let ctx = ttrpc::context::with_duration(Duration::from_secs(10));
        let _ = agent.delete_container(ctx, &del_req).await;

        // Drop agent/ttrpc connection before cleanup
        drop(agent);
        drop(_health);

        vm.cleanup().await.expect("VM cleanup failed");
        assert!(!vm.state_dir().exists(), "state dir should be removed");

        // Disk image cleanup
        let _ = std::fs::remove_file(&disk_path);
        let _ = std::fs::remove_dir_all(&bundle_tmp);

        eprintln!("╚══════════════════════════════════════════════════════════╝\n");
    });
}

/// Run a command, panic on failure.
fn run_cmd(cmd: &str, args: &[&str]) {
    let status = std::process::Command::new(cmd)
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("{cmd} {}: {e}", args.join(" ")));
    assert!(
        status.success(),
        "{cmd} {} failed: {status}",
        args.join(" ")
    );
}

/// Run a command inside a network namespace, panic on failure.
fn run_nsenter(netns_path: &str, args: &[&str]) {
    let net_arg = format!("--net={netns_path}");
    let mut cmd_args = vec!["nsenter", &net_arg, "--"];
    cmd_args.extend(args);
    let status = std::process::Command::new(cmd_args[0])
        .args(&cmd_args[1..])
        .status()
        .unwrap_or_else(|e| panic!("nsenter {:?}: {e}", args));
    assert!(status.success(), "nsenter {:?} failed: {status}", args);
}

/// Run a command, return success status (no panic).
fn run_cmd_status(cmd: &str, args: &[&str]) -> bool {
    std::process::Command::new(cmd)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Helper: create an ext4 disk image containing an http-echo binary + OCI config.
/// Returns the disk image path.
fn create_echo_disk_image(
    state_dir: &std::path::Path,
    http_echo_path: &std::path::Path,
    name: &str,
    text: &str,
    port: u16,
) -> std::path::PathBuf {
    let disk_path = state_dir.join(format!("{name}.img"));
    let bundle_tmp = state_dir.join(format!("{name}-bundle"));
    let rootfs_tmp = bundle_tmp.join("rootfs");
    std::fs::create_dir_all(&rootfs_tmp).expect("mkdir rootfs");
    std::fs::copy(http_echo_path, rootfs_tmp.join("http-echo")).expect("cp http-echo");
    std::process::Command::new("chmod")
        .args(["755", &rootfs_tmp.join("http-echo").to_string_lossy()])
        .status()
        .expect("chmod");

    let oci_config = serde_json::json!({
        "ociVersion": "1.0.2",
        "process": {
            "terminal": false,
            "user": { "uid": 0, "gid": 0 },
            "args": ["/http-echo", &format!("-text={text}"), &format!("-listen=:{port}")],
            "env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
            "cwd": "/"
        },
        "root": { "path": "rootfs", "readonly": false },
        "linux": { "namespaces": [{"type": "pid"}, {"type": "mount"}] }
    });
    std::fs::write(
        bundle_tmp.join("config.json"),
        serde_json::to_string_pretty(&oci_config).unwrap(),
    )
    .expect("write config.json");

    // Create ext4 disk image
    let f = std::fs::File::create(&disk_path).expect("create disk");
    f.set_len(64 * 1024 * 1024).expect("set_len");
    drop(f);
    assert!(run_cmd_status(
        "mkfs.ext4",
        &["-q", "-F", &disk_path.to_string_lossy()]
    ));
    let mount_dir = disk_path.with_extension("mnt");
    std::fs::create_dir_all(&mount_dir).expect("mkdir mount");
    assert!(run_cmd_status(
        "mount",
        &[
            "-o",
            "loop",
            &disk_path.to_string_lossy(),
            &mount_dir.to_string_lossy()
        ]
    ));
    let img_rootfs = mount_dir.join("rootfs");
    std::fs::create_dir_all(&img_rootfs).expect("mkdir img rootfs");
    assert!(run_cmd_status(
        "cp",
        &[
            "-a",
            "--",
            &format!("{}/.", rootfs_tmp.display()),
            &img_rootfs.to_string_lossy()
        ]
    ));
    std::fs::copy(
        bundle_tmp.join("config.json"),
        mount_dir.join("config.json"),
    )
    .expect("cp config.json");
    assert!(run_cmd_status("umount", &[&mount_dir.to_string_lossy()]));
    std::fs::remove_dir(&mount_dir).ok();
    std::fs::remove_dir_all(&bundle_tmp).ok();

    disk_path
}

/// Test that two containers can run simultaneously inside the same VM.
///
/// This proves multi-container-per-VM isolation:
///   1. Boot a single VM with networking
///   2. Hot-plug two disk images (each with http-echo on a different port)
///   3. Start both containers via the agent
///   4. Curl both endpoints from the host and verify independent responses
///   5. Both containers share the VM's network namespace (same IP, different ports)
///
/// Requires: root, KVM, CH, kernel, rootfs, http-echo binary
#[test]
fn test_two_containers_in_one_vm() {
    let fixtures = TestFixtures::resolve().expect("failed to resolve test fixtures");
    skip_if_missing!(fixtures);

    let http_echo_path = std::env::var("CLOUDHV_TEST_HTTP_ECHO")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/usr/local/bin/http-echo"));
    if !http_echo_path.exists() {
        eprintln!("SKIPPING: http-echo not found (set CLOUDHV_TEST_HTTP_ECHO)");
        return;
    }
    for tool in ["nsenter", "ip", "tc", "curl", "mkfs.ext4"] {
        if !run_cmd_status("which", &[tool]) {
            eprintln!("SKIPPING: {tool} not found");
            return;
        }
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let vm_id = format!("multi-ctr-{}", std::process::id());
        let mut config = fixtures.runtime_config();
        config.max_containers_per_vm = 5;

        // Networking setup (same pattern as test_echo_container_with_networking)
        let netns_name = format!("chmulti-{}", std::process::id());
        let netns_path = format!("/var/run/netns/{netns_name}");
        let veth_host = "veth-multi-h";
        let veth_ns = "veth-multi-ns";
        let test_ip = "10.200.202.2";
        let host_ip = "10.200.202.1";
        let netmask = "255.255.255.0";

        struct Guard {
            name: String,
            veth: String,
        }
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::process::Command::new("ip")
                    .args(["link", "delete", &self.veth])
                    .status();
                let _ = std::process::Command::new("ip")
                    .args(["netns", "delete", &self.name])
                    .status();
            }
        }
        let _guard = Guard {
            name: netns_name.clone(),
            veth: veth_host.to_string(),
        };

        eprintln!("\n╔══════════════════════════════════════════════════════════╗");
        eprintln!("║  Two Containers in One VM — Integration Test            ║");
        eprintln!("╠══════════════════════════════════════════════════════════╣");

        // ── Phase 1: Boot VM with networking ──────────────────────────
        eprintln!("  Phase 1 │ Booting VM with networking");

        // Set up netns + veth + TAP + TC redirect
        run_cmd("ip", &["netns", "add", &netns_name]);
        run_cmd(
            "ip",
            &[
                "link", "add", veth_host, "type", "veth", "peer", "name", veth_ns,
            ],
        );
        run_cmd("ip", &["link", "set", veth_ns, "netns", &netns_name]);
        run_cmd(
            "ip",
            &["addr", "add", &format!("{host_ip}/24"), "dev", veth_host],
        );
        run_cmd("ip", &["link", "set", veth_host, "up"]);
        run_nsenter(
            &netns_path,
            &[
                "ip",
                "addr",
                "add",
                &format!("{test_ip}/24"),
                "dev",
                veth_ns,
            ],
        );
        run_nsenter(&netns_path, &["ip", "link", "set", veth_ns, "up"]);
        run_nsenter(&netns_path, &["ip", "link", "set", "lo", "up"]);
        run_nsenter(
            &netns_path,
            &["ip", "route", "add", "default", "via", host_ip],
        );

        let tap_name = format!("tap_{}", &vm_id[..8.min(vm_id.len())]);
        run_nsenter(
            &netns_path,
            &["ip", "tuntap", "add", &tap_name, "mode", "tap"],
        );
        run_nsenter(&netns_path, &["ip", "link", "set", &tap_name, "up"]);

        let mac_out = std::process::Command::new("nsenter")
            .args([
                &format!("--net={netns_path}"),
                "--",
                "ip",
                "-j",
                "addr",
                "show",
                "dev",
                veth_ns,
            ])
            .output()
            .expect("get MAC");
        let addrs: serde_json::Value =
            serde_json::from_slice(&mac_out.stdout).unwrap_or(serde_json::json!([]));
        let tap_mac = addrs
            .as_array()
            .and_then(|a| a.first())
            .and_then(|i| i.get("address"))
            .and_then(|a| a.as_str())
            .unwrap_or("aa:bb:cc:dd:ee:ff")
            .to_string();

        run_nsenter(
            &netns_path,
            &["tc", "qdisc", "add", "dev", veth_ns, "ingress"],
        );
        run_nsenter(
            &netns_path,
            &[
                "tc", "filter", "add", "dev", veth_ns, "parent", "ffff:", "protocol", "all", "u32",
                "match", "u32", "0", "0", "action", "mirred", "egress", "redirect", "dev",
                &tap_name,
            ],
        );
        run_nsenter(
            &netns_path,
            &["tc", "qdisc", "add", "dev", &tap_name, "ingress"],
        );
        run_nsenter(
            &netns_path,
            &[
                "tc", "filter", "add", "dev", &tap_name, "parent", "ffff:", "protocol", "all",
                "u32", "match", "u32", "0", "0", "action", "mirred", "egress", "redirect", "dev",
                veth_ns,
            ],
        );
        run_nsenter(&netns_path, &["ip", "addr", "flush", "dev", veth_ns]);

        // Boot VM with networking
        config.kernel_args = format!(
            "{} ip={test_ip}::{host_ip}:{netmask}::eth0:off",
            config.kernel_args
        );

        let mut vm = containerd_shim_cloudhv::vm::VmManager::new(vm_id.clone(), config)
            .expect("VmManager::new");
        vm.prepare().await.expect("prepare");
        vm.spawn_vmm_in_netns(Some(&netns_path))
            .expect("spawn_vmm_in_netns");
        vm.wait_vmm_ready().await.expect("vmm ready");
        vm.create_and_boot_vm(Some(&tap_name), Some(&tap_mac))
            .await
            .expect("create_and_boot_vm");
        tokio::time::timeout(Duration::from_secs(30), vm.wait_for_agent())
            .await
            .expect("agent timeout")
            .expect("agent health");
        eprintln!("           │ VM booted at {test_ip}");

        // Connect ttrpc
        let vsock_client = containerd_shim_cloudhv::vsock::VsockClient::new(vm.vsock_socket());
        let (agent, _health) =
            tokio::time::timeout(Duration::from_secs(5), vsock_client.connect_ttrpc())
                .await
                .expect("ttrpc timeout")
                .expect("ttrpc connect");

        // ── Phase 2: Hot-plug and start container A (port 5678) ───────
        eprintln!("  Phase 2 │ Starting container A (port 5678)");
        let ctr_a_id = format!("ctr-a-{}", std::process::id());
        let disk_a = create_echo_disk_image(
            vm.state_dir(),
            &http_echo_path,
            "ctr-a",
            "response-from-container-A",
            5678,
        );
        vm.add_disk(&disk_a.to_string_lossy(), &ctr_a_id, false)
            .await
            .expect("add_disk A");

        let io_a = vm.shared_dir().join("io").join(&ctr_a_id);
        std::fs::create_dir_all(&io_a).expect("mkdir io A");
        let mut req_a = cloudhv_proto::CreateContainerRequest::new();
        req_a.container_id = ctr_a_id.clone();
        req_a.bundle_path = format!("/run/containers/{}", ctr_a_id);
        let ctx = ttrpc::context::with_duration(Duration::from_secs(30));
        agent.create_container(ctx, &req_a).await.expect("create A");
        let mut start_a = cloudhv_proto::StartContainerRequest::new();
        start_a.container_id = ctr_a_id.clone();
        let ctx = ttrpc::context::with_duration(Duration::from_secs(30));
        agent.start_container(ctx, &start_a).await.expect("start A");
        eprintln!("           │ container A started");

        // ── Phase 3: Hot-plug and start container B (port 5679) ───────
        eprintln!("  Phase 3 │ Starting container B (port 5679)");
        let ctr_b_id = format!("ctr-b-{}", std::process::id());
        let disk_b = create_echo_disk_image(
            vm.state_dir(),
            &http_echo_path,
            "ctr-b",
            "response-from-container-B",
            5679,
        );
        vm.add_disk(&disk_b.to_string_lossy(), &ctr_b_id, false)
            .await
            .expect("add_disk B");

        let io_b = vm.shared_dir().join("io").join(&ctr_b_id);
        std::fs::create_dir_all(&io_b).expect("mkdir io B");
        let mut req_b = cloudhv_proto::CreateContainerRequest::new();
        req_b.container_id = ctr_b_id.clone();
        req_b.bundle_path = format!("/run/containers/{}", ctr_b_id);
        let ctx = ttrpc::context::with_duration(Duration::from_secs(30));
        agent.create_container(ctx, &req_b).await.expect("create B");
        let mut start_b = cloudhv_proto::StartContainerRequest::new();
        start_b.container_id = ctr_b_id.clone();
        let ctx = ttrpc::context::with_duration(Duration::from_secs(30));
        agent.start_container(ctx, &start_b).await.expect("start B");
        eprintln!("           │ container B started");

        // Give both containers time to bind their ports
        tokio::time::sleep(Duration::from_secs(2)).await;

        // ── Phase 4: Curl both containers ─────────────────────────────
        eprintln!("  Phase 4 │ Verifying both containers respond");

        let resp_a = std::process::Command::new("curl")
            .args([
                "-s",
                "--connect-timeout",
                "5",
                &format!("http://{test_ip}:5678/"),
            ])
            .output()
            .expect("curl A");
        let body_a = String::from_utf8_lossy(&resp_a.stdout).trim().to_string();
        eprintln!("           │ container A: \"{body_a}\"");
        assert!(resp_a.status.success(), "curl A failed");
        assert!(
            body_a.contains("response-from-container-A"),
            "expected container A response, got: {body_a}"
        );

        let resp_b = std::process::Command::new("curl")
            .args([
                "-s",
                "--connect-timeout",
                "5",
                &format!("http://{test_ip}:5679/"),
            ])
            .output()
            .expect("curl B");
        let body_b = String::from_utf8_lossy(&resp_b.stdout).trim().to_string();
        eprintln!("           │ container B: \"{body_b}\"");
        assert!(resp_b.status.success(), "curl B failed");
        assert!(
            body_b.contains("response-from-container-B"),
            "expected container B response, got: {body_b}"
        );

        eprintln!("           │ ✅ Both containers respond independently!");

        // ── Phase 5: Cleanup ──────────────────────────────────────────
        eprintln!("  Phase 5 │ Cleaning up");
        drop(agent);
        drop(_health);
        vm.cleanup().await.expect("cleanup");
        let _ = std::fs::remove_file(&disk_a);
        let _ = std::fs::remove_file(&disk_b);

        eprintln!("╚══════════════════════════════════════════════════════════╝\n");
    });
}

/// Test that a VM can be resized to grow memory when hotplug is configured.
///
/// This verifies the memory growth path:
///   1. Boot VM with 128MB + 256MB hotplug headroom (virtio-mem)
///   2. Query initial memory via agent GetMemInfo
///   3. Resize VM to 256MB via vm.resize
///   4. Query memory again and verify it increased
#[test]
fn test_vm_memory_growth() {
    let fixtures = TestFixtures::resolve().expect("failed to resolve test fixtures");
    skip_if_missing!(fixtures);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let mut config = fixtures.runtime_config();
        config.default_memory_mb = 128;
        config.hotplug_memory_mb = 256;
        config.hotplug_method = "virtio-mem".to_string();
        let vm_id = format!("mem-grow-{}", std::process::id());

        eprintln!("\n╔══════════════════════════════════════════════════════════╗");
        eprintln!("║  VM Memory Growth — Integration Test                    ║");
        eprintln!("╠══════════════════════════════════════════════════════════╣");

        let mut vm = containerd_shim_cloudhv::vm::VmManager::new(vm_id, config).expect("VmManager");
        vm.prepare().await.expect("prepare");
        vm.spawn_vmm_in_netns(None).expect("vmm spawn failed");
        vm.wait_vmm_ready().await.expect("vmm ready failed");
        vm.create_and_boot_vm(None, None).await.expect("boot");
        tokio::time::timeout(Duration::from_secs(30), vm.wait_for_agent())
            .await
            .expect("agent timeout")
            .expect("agent");

        // Query initial memory
        let vsock = containerd_shim_cloudhv::vsock::VsockClient::new(vm.vsock_socket());
        let (agent, _health) = vsock.connect_ttrpc().await.expect("ttrpc");
        let ctx = ttrpc::context::with_duration(Duration::from_secs(5));
        let initial = agent
            .get_mem_info(ctx, &cloudhv_proto::GetMemInfoRequest::new())
            .await
            .expect("initial meminfo");
        let initial_total_mb = initial.mem_total_kb / 1024;
        eprintln!("  Initial │ MemTotal: {}MiB", initial_total_mb);
        assert!(
            (100..=160).contains(&initial_total_mb),
            "expected ~128MiB, got {}MiB",
            initial_total_mb
        );

        // Resize to 256MB
        eprintln!("  Resize  │ 128MiB -> 256MiB");
        vm.resize(None, Some(256 * 1024 * 1024))
            .await
            .expect("resize to 256MiB must succeed");
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Query again
        let vsock2 = containerd_shim_cloudhv::vsock::VsockClient::new(vm.vsock_socket());
        let (agent2, _h2) = vsock2.connect_ttrpc().await.expect("ttrpc2");
        let ctx = ttrpc::context::with_duration(Duration::from_secs(5));
        let grown = agent2
            .get_mem_info(ctx, &cloudhv_proto::GetMemInfoRequest::new())
            .await
            .expect("grown meminfo");
        let grown_total_mb = grown.mem_total_kb / 1024;
        eprintln!("  After   │ MemTotal: {}MiB", grown_total_mb);
        assert!(
            grown_total_mb > initial_total_mb,
            "VM memory did not grow after resize: got {}MiB (was {}MiB). \
             Kernel may lack CONFIG_VIRTIO_MEM or CONFIG_MEMORY_HOTPLUG.",
            grown_total_mb,
            initial_total_mb
        );
        eprintln!("           │ ✅ Memory grew successfully");

        drop(agent);
        drop(_health);
        drop(agent2);
        drop(_h2);
        vm.cleanup().await.expect("cleanup");
        eprintln!("╚══════════════════════════════════════════════════════════╝\n");
    });
}

/// Test that a VM's memory can be shrunk back after growth (virtio-mem).
///
/// This verifies the reclaim path:
///   1. Boot VM with 128MB + 256MB hotplug (virtio-mem)
///   2. Grow to 256MB
///   3. Shrink back to 128MB
///   4. Verify MemTotal decreased
#[test]
fn test_vm_memory_reclaim() {
    let fixtures = TestFixtures::resolve().expect("failed to resolve test fixtures");
    skip_if_missing!(fixtures);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let mut config = fixtures.runtime_config();
        config.default_memory_mb = 128;
        config.hotplug_memory_mb = 256;
        config.hotplug_method = "virtio-mem".to_string();
        let vm_id = format!("mem-reclaim-{}", std::process::id());

        eprintln!("\n╔══════════════════════════════════════════════════════════╗");
        eprintln!("║  VM Memory Reclaim — Integration Test                   ║");
        eprintln!("╠══════════════════════════════════════════════════════════╣");

        let mut vm = containerd_shim_cloudhv::vm::VmManager::new(vm_id, config).expect("VmManager");
        vm.prepare().await.expect("prepare");
        vm.spawn_vmm_in_netns(None).expect("vmm spawn failed");
        vm.wait_vmm_ready().await.expect("vmm ready failed");
        vm.create_and_boot_vm(None, None).await.expect("boot");
        tokio::time::timeout(Duration::from_secs(30), vm.wait_for_agent())
            .await
            .expect("agent timeout")
            .expect("agent");

        // Grow to 256MB
        eprintln!("  Phase 1 │ Growing 128MiB -> 256MiB");
        vm.resize(None, Some(256 * 1024 * 1024))
            .await
            .expect("resize to 256MiB must succeed");
        tokio::time::sleep(Duration::from_secs(2)).await;

        let vsock = containerd_shim_cloudhv::vsock::VsockClient::new(vm.vsock_socket());
        let (agent, _h) = vsock.connect_ttrpc().await.expect("ttrpc");
        let ctx = ttrpc::context::with_duration(Duration::from_secs(5));
        let grown = agent
            .get_mem_info(ctx, &cloudhv_proto::GetMemInfoRequest::new())
            .await
            .expect("grown meminfo");
        let grown_mb = grown.mem_total_kb / 1024;
        eprintln!("           │ MemTotal: {}MiB", grown_mb);
        assert!(
            grown_mb > 120,
            "growth didn't take effect: {}MiB. Kernel needs CONFIG_VIRTIO_MEM.",
            grown_mb
        );
        drop(agent);
        drop(_h);

        // Shrink back to 128MB
        eprintln!("  Phase 2 │ Shrinking 256MiB -> 128MiB");
        vm.resize(None, Some(128 * 1024 * 1024))
            .await
            .expect("resize to 128MiB must succeed");
        tokio::time::sleep(Duration::from_secs(2)).await;

        let vsock2 = containerd_shim_cloudhv::vsock::VsockClient::new(vm.vsock_socket());
        let (agent2, _h2) = vsock2.connect_ttrpc().await.expect("ttrpc2");
        let ctx = ttrpc::context::with_duration(Duration::from_secs(5));
        let shrunk = agent2
            .get_mem_info(ctx, &cloudhv_proto::GetMemInfoRequest::new())
            .await
            .expect("shrunk meminfo");
        let shrunk_mb = shrunk.mem_total_kb / 1024;
        eprintln!("           │ MemTotal: {}MiB", shrunk_mb);
        assert!(
            shrunk_mb < grown_mb,
            "VM memory did not shrink after resize: got {}MiB (was {}MiB). \
             Kernel needs CONFIG_MEMORY_HOTREMOVE + CONFIG_VIRTIO_MEM.",
            shrunk_mb,
            grown_mb
        );
        eprintln!("           │ ✅ Memory successfully reclaimed");

        drop(agent2);
        drop(_h2);
        vm.cleanup().await.expect("cleanup");
        eprintln!("╚══════════════════════════════════════════════════════════╝\n");
    });
}

/// Test dual-path volume support: filesystem volumes via shared dir and
/// block volumes via vm.add-disk.
///
/// Filesystem path:
///   1. Write test data to a directory in the shared dir
///   2. Boot a container that reads from that directory
///   3. Verify the container sees the data
///
/// Block path:
///   1. Create a small ext4 disk image with test data
///   2. Hot-plug it into the VM via vm.add-disk
///   3. Verify the agent can discover the new block device
///
/// Requires: root, KVM, CH, kernel, rootfs, http-echo
#[test]
fn test_volume_mounts() {
    let fixtures = TestFixtures::resolve().expect("failed to resolve test fixtures");
    skip_if_missing!(fixtures);

    // Need busybox/sh to build a container that reads volume data
    let shell_src = if std::path::Path::new("/bin/busybox").exists() {
        std::path::PathBuf::from("/bin/busybox")
    } else if std::path::Path::new("/bin/sh").exists() {
        std::path::PathBuf::from("/bin/sh")
    } else {
        eprintln!("SKIPPING: neither /bin/busybox nor /bin/sh found");
        return;
    };

    for tool in ["mkfs.ext4", "cp"] {
        if !run_cmd_status("which", &[tool]) {
            eprintln!("SKIPPING: {tool} not found");
            return;
        }
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let config = fixtures.runtime_config();
        let vm_id = format!("vol-test-{}", std::process::id());
        let container_id = format!("vol-ctr-{}", std::process::id());

        eprintln!("\n╔══════════════════════════════════════════════════════════╗");
        eprintln!("║  Baked-in Volume Mounts — Integration Test              ║");
        eprintln!("╠══════════════════════════════════════════════════════════╣");

        // ── Phase 1: Boot VM ──────────────────────────────────────────
        eprintln!("  Phase 1 │ Booting VM");
        let mut vm =
            containerd_shim_cloudhv::vm::VmManager::new(vm_id.clone(), config).expect("VmManager");
        vm.prepare().await.expect("prepare");
        vm.spawn_vmm_in_netns(None).expect("vmm spawn failed");
        vm.wait_vmm_ready().await.expect("vmm ready failed");
        vm.create_and_boot_vm(None, None).await.expect("boot");
        tokio::time::timeout(Duration::from_secs(30), vm.wait_for_agent())
            .await
            .expect("agent timeout")
            .expect("agent");
        eprintln!("           │ VM booted");

        // ── Phase 2: Create disk with baked-in ConfigMap volume ───────
        eprintln!("  Phase 2 │ Creating disk image with baked-in ConfigMap");

        // Simulate kubelet ConfigMap volume data directory
        let cm_src = vm.state_dir().join("configmap-data");
        std::fs::create_dir_all(&cm_src).expect("mkdir configmap");
        std::fs::write(cm_src.join("app.conf"), "setting=BAKED_VALUE\n").expect("write cm");
        std::fs::write(cm_src.join("extra.yaml"), "env: production\n").expect("write cm2");

        // Compute volume_id the same way the shim does (djb2 hash)
        let vol_dest = "/etc/config";
        let vol_id = {
            let mut h: u64 = 5381;
            for b in vol_dest.bytes() {
                h = h.wrapping_mul(33).wrapping_add(b as u64);
            }
            format!("{:x}", h)
        };

        // Build an OCI bundle with the container command that reads volume data
        let bundle_dir = vm.state_dir().join(format!("{container_id}-bundle"));
        let rootfs = bundle_dir.join("rootfs");
        let bin_dir = rootfs.join("bin");
        std::fs::create_dir_all(&bin_dir).expect("mkdir bin");
        std::fs::copy(&shell_src, bin_dir.join("sh")).expect("cp shell");
        std::process::Command::new("chmod")
            .args(["755", &bin_dir.join("sh").to_string_lossy()])
            .status()
            .expect("chmod");
        if shell_src.to_string_lossy().contains("busybox") {
            for applet in &["cat", "echo", "ls"] {
                let _ = std::os::unix::fs::symlink("sh", bin_dir.join(applet));
            }
        }

        // The container will cat the baked-in ConfigMap files
        let oci_config = serde_json::json!({
            "ociVersion": "1.0.2",
            "process": {
                "terminal": false,
                "user": { "uid": 0, "gid": 0 },
                "args": ["sh", "-c",
                    "cat /etc/config/app.conf && cat /etc/config/extra.yaml"],
                "env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
                "cwd": "/"
            },
            "root": { "path": "rootfs", "readonly": false },
            "linux": { "namespaces": [{"type": "pid"}, {"type": "mount"}] },
            "mounts": [
                {
                    "destination": vol_dest,
                    "source": cm_src.to_string_lossy(),
                    "type": "bind",
                    "options": ["rbind", "ro"]
                }
            ]
        });
        std::fs::write(
            bundle_dir.join("config.json"),
            serde_json::to_string_pretty(&oci_config).unwrap(),
        )
        .expect("write config.json");

        // Stage the disk image: rootfs + volumes/<vol_id>/ (same as create_rootfs_disk_image)
        let disk_path = vm.state_dir().join(format!("{container_id}.img"));
        let staging = disk_path.with_extension("staging");
        std::fs::create_dir_all(staging.join("rootfs")).expect("mkdir staging/rootfs");
        assert!(run_cmd_status(
            "cp",
            &[
                "-a",
                "--",
                &format!("{}/.", rootfs.display()),
                &staging.join("rootfs").to_string_lossy()
            ]
        ));
        std::fs::copy(bundle_dir.join("config.json"), staging.join("config.json"))
            .expect("cp config.json");

        // Bake ConfigMap data into volumes/<vol_id>/
        let vol_staging = staging.join("volumes").join(&vol_id);
        std::fs::create_dir_all(&vol_staging).expect("mkdir vol staging");
        assert!(run_cmd_status(
            "cp",
            &[
                "-a",
                "--",
                &format!("{}/.", cm_src.display()),
                &vol_staging.to_string_lossy()
            ]
        ));

        // Create ext4 image with mkfs.ext4 -d (no loopback mount)
        let f = std::fs::File::create(&disk_path).expect("create disk");
        f.set_len(64 * 1024 * 1024).expect("set_len"); // 64MB
        drop(f);
        assert!(run_cmd_status(
            "mkfs.ext4",
            &[
                "-q",
                "-F",
                "-d",
                &staging.to_string_lossy(),
                &disk_path.to_string_lossy()
            ]
        ));
        std::fs::remove_dir_all(&staging).ok();
        eprintln!("           │ Disk image with baked ConfigMap created");

        // ── Phase 3: Hot-plug disk and run container ─────────────────
        eprintln!("  Phase 3 │ Hot-plugging disk and running container");

        let disk_id = format!("ctr-{}", &container_id[..12.min(container_id.len())]);
        vm.add_disk(&disk_path.to_string_lossy(), &disk_id, false)
            .await
            .expect("add_disk");

        let vsock = containerd_shim_cloudhv::vsock::VsockClient::new(vm.vsock_socket());
        let (agent, _health) = vsock.connect_ttrpc().await.expect("ttrpc");

        // Send CreateContainer with FILESYSTEM volume mount pointing to baked-in data
        let bundle_guest = format!("/run/containers/{}", container_id);
        let mut create_req = cloudhv_proto::CreateContainerRequest::new();
        create_req.container_id = container_id.clone();
        create_req.bundle_path = bundle_guest.clone();

        // Add the ConfigMap as a FILESYSTEM volume (baked into disk)
        let mut vol_mount = cloudhv_proto::VolumeMount::new();
        vol_mount.destination = vol_dest.to_string();
        vol_mount.source = format!("{}/volumes/{}", bundle_guest, vol_id);
        vol_mount.volume_type = cloudhv_proto::VolumeType::FILESYSTEM.into();
        vol_mount.readonly = true;
        create_req.volumes.push(vol_mount);

        let ctx = ttrpc::context::with_duration(Duration::from_secs(30));
        agent
            .create_container(ctx, &create_req)
            .await
            .expect("CreateContainer");

        let mut start_req = cloudhv_proto::StartContainerRequest::new();
        start_req.container_id = container_id.clone();
        let ctx = ttrpc::context::with_duration(Duration::from_secs(30));
        agent
            .start_container(ctx, &start_req)
            .await
            .expect("StartContainer");

        // Wait for the container to finish
        let mut wait_req = cloudhv_proto::WaitContainerRequest::new();
        wait_req.container_id = container_id.clone();
        let ctx = ttrpc::context::with_duration(Duration::from_secs(60));
        let wait_resp = agent
            .wait_container(ctx, &wait_req)
            .await
            .expect("WaitContainer");
        assert_eq!(
            wait_resp.exit_status, 0,
            "container should exit 0, got {}",
            wait_resp.exit_status
        );
        eprintln!("           │ Container exited with status 0");

        // ── Phase 4: Verify volume data via container logs ────────────
        eprintln!("  Phase 4 │ Verifying ConfigMap data in container output");

        let mut log_req = cloudhv_proto::GetContainerLogsRequest::new();
        log_req.container_id = container_id.clone();
        log_req.offset = 0;
        let ctx = ttrpc::context::with_duration(Duration::from_secs(5));
        let log_resp = agent
            .get_container_logs(ctx, &log_req)
            .await
            .expect("GetContainerLogs");

        let stdout = String::from_utf8_lossy(&log_resp.stdout);
        assert!(
            stdout.contains("setting=BAKED_VALUE"),
            "stdout should contain ConfigMap data 'setting=BAKED_VALUE', got: {stdout:?}"
        );
        assert!(
            stdout.contains("env: production"),
            "stdout should contain ConfigMap data 'env: production', got: {stdout:?}"
        );
        eprintln!("           │ ✅ ConfigMap data (app.conf) verified in container output");
        eprintln!("           │ ✅ ConfigMap data (extra.yaml) verified in container output");

        // ── Phase 5: Cleanup ──────────────────────────────────────────
        eprintln!("  Phase 5 │ Cleaning up");
        let mut del_req = cloudhv_proto::DeleteContainerRequest::new();
        del_req.container_id = container_id.clone();
        let ctx = ttrpc::context::with_duration(Duration::from_secs(10));
        let _ = agent.delete_container(ctx, &del_req).await;

        drop(agent);
        drop(_health);
        vm.cleanup().await.expect("cleanup");
        let _ = std::fs::remove_file(&disk_path);
        let _ = std::fs::remove_dir_all(&bundle_dir);

        eprintln!("╚══════════════════════════════════════════════════════════╝\n");
    });
}
