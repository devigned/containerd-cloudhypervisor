#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::Result;
#[cfg(target_os = "linux")]
use log::debug;
use log::info;

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
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
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

    info!(
        "mounting virtio-fs: tag={} at {}",
        VIRTIOFS_TAG, mount_point
    );

    mount(
        Some(VIRTIOFS_TAG),
        mount_point,
        Some("virtiofs"),
        MsFlags::empty(),
        None::<&str>,
    )
    .with_context(|| format!("failed to mount virtio-fs tag={VIRTIOFS_TAG} at {mount_point}"))?;

    info!("virtio-fs mounted at {}", mount_point);
    Ok(())
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
