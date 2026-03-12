#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::Result;
#[cfg(target_os = "linux")]
use log::debug;
use log::info;
use log::warn;

#[cfg(target_os = "linux")]
use cloudhv_common::{VIRTIOFS_GUEST_MOUNT, VIRTIOFS_TAG};

/// Mount essential filesystems for a minimal init environment.
///
/// Called when the agent runs as PID 1 inside the VM.
/// Only available on Linux.
#[cfg(target_os = "linux")]
pub fn mount_essential_filesystems() -> Result<()> {
    use nix::mount::MsFlags;

    info!("mounting essential filesystems");

    mount_if_needed(
        "proc",
        "/proc",
        "proc",
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
    )?;
    mount_if_needed(
        "sysfs",
        "/sys",
        "sysfs",
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
    )?;
    mount_if_needed("devtmpfs", "/dev", "devtmpfs", MsFlags::MS_NOSUID)?;

    // Ensure standard device nodes exist with correct permissions.
    // devtmpfs should auto-create these, but if the kernel's devtmpfs
    // mount doesn't populate them, create them manually.
    create_dev_node("/dev/null", 1, 3, 0o666);
    create_dev_node("/dev/zero", 1, 5, 0o666);
    create_dev_node("/dev/full", 1, 7, 0o666);
    create_dev_node("/dev/random", 1, 8, 0o666);
    create_dev_node("/dev/urandom", 1, 9, 0o666);
    create_dev_node("/dev/tty", 5, 0, 0o666);

    std::fs::create_dir_all("/dev/pts").ok();
    mount_if_needed(
        "devpts",
        "/dev/pts",
        "devpts",
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
    )?;

    std::fs::create_dir_all("/dev/shm").ok();
    mount_if_needed(
        "tmpfs",
        "/dev/shm",
        "tmpfs",
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
    )?;

    mount_if_needed(
        "tmpfs",
        "/tmp",
        "tmpfs",
        MsFlags::MS_NOSUID, // No MS_NODEV — container rootfs copies need device nodes
    )?;

    std::fs::create_dir_all("/run").ok();
    mount_if_needed(
        "tmpfs",
        "/run",
        "tmpfs",
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
    )?;

    std::fs::create_dir_all("/sys/fs/cgroup").ok();
    mount_if_needed(
        "cgroup2",
        "/sys/fs/cgroup",
        "cgroup2",
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
    )?;

    info!("essential filesystems mounted");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn mount_essential_filesystems() -> Result<()> {
    info!("mount_essential_filesystems: no-op on non-Linux");
    Ok(())
}

/// Mount the virtio-fs shared filesystem from the host.
#[cfg(target_os = "linux")]
pub fn mount_virtiofs() -> Result<()> {
    use nix::mount::{mount, MsFlags};

    let mount_point = VIRTIOFS_GUEST_MOUNT;
    std::fs::create_dir_all(mount_point)
        .with_context(|| format!("failed to create mount point: {mount_point}"))?;

    // Retry the mount — the virtio-fs device may not be ready immediately
    // after the kernel boots (vhost-user handshake with virtiofsd is async).
    for attempt in 1..=10 {
        match mount(
            Some(VIRTIOFS_TAG),
            mount_point,
            Some("virtiofs"),
            MsFlags::empty(),
            None::<&str>,
        ) {
            Ok(()) => {
                info!("virtio-fs mounted at {} (attempt {})", mount_point, attempt);
                return Ok(());
            }
            Err(e) if attempt < 10 => {
                warn!(
                    "virtio-fs mount attempt {}/10 failed: {} (retrying in 100ms)",
                    attempt, e
                );
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                anyhow::bail!(
                    "failed to mount virtio-fs tag={} at {} after 10 attempts: {}",
                    VIRTIOFS_TAG,
                    mount_point,
                    e
                );
            }
        }
    }
    unreachable!()
}

#[cfg(not(target_os = "linux"))]
pub fn mount_virtiofs() -> Result<()> {
    info!("mount_virtiofs: no-op on non-Linux");
    Ok(())
}

/// Helper: mount a filesystem if the target is not already mounted.
#[cfg(target_os = "linux")]
fn mount_if_needed(
    source: &str,
    target: &str,
    fstype: &str,
    flags: nix::mount::MsFlags,
) -> Result<()> {
    use nix::mount::mount;

    if is_mounted(target) {
        debug!("{} already mounted at {}", fstype, target);
        return Ok(());
    }

    std::fs::create_dir_all(target).ok();

    mount(Some(source), target, Some(fstype), flags, None::<&str>)
        .with_context(|| format!("failed to mount {fstype} at {target}"))?;

    debug!("mounted {} at {}", fstype, target);
    Ok(())
}

/// Check if a path is a mount point by reading /proc/mounts.
#[cfg(target_os = "linux")]
fn is_mounted(target: &str) -> bool {
    std::fs::read_to_string("/proc/mounts")
        .map(|mounts| {
            mounts
                .lines()
                .any(|line| line.split_whitespace().nth(1) == Some(target))
        })
        .unwrap_or(false)
}

/// Create a device node if it doesn't exist.
#[cfg(target_os = "linux")]
#[allow(dead_code)]
pub fn create_dev_node(path: &str, major: u64, minor: u64, mode: u32) {
    use std::path::Path;
    let dev = nix::sys::stat::makedev(major, minor);
    let cpath = std::ffi::CString::new(path).unwrap();

    // Save and clear umask so mknod creates nodes with exact permissions
    let old_umask = unsafe { libc::umask(0) };

    if Path::new(path).exists() {
        unsafe {
            libc::chmod(cpath.as_ptr(), mode);
        }
    } else {
        unsafe {
            libc::mknod(cpath.as_ptr(), libc::S_IFCHR | mode, dev);
        }
    }

    unsafe {
        libc::umask(old_umask);
    }
}

#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
pub fn create_dev_node(_path: &str, _major: u64, _minor: u64, _mode: u32) {}
