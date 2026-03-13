# Containerised build hack scripts

Scripts are meant to be called from the repository root.

* `hacks/build-guest-kernel.sh` - build the guest kernel in an ephemeral Alpine container.
  - will put `vmlinux` and `vmlinux.kconfig` in the repo root after build.
* `hacks/build-static-rust.sh` - statically compile Rust binaries from this repo.
  - e.g. `hacks/build-static.sh containerd-shim-cloudhv crates/agent/cloudhv-agent` will build both L1 containerd shim and L2 cloud hypervisor agent.
  - will put the build result (static binary) into the repo root.
* `hacks/build-guest.sh` - Build a full guest, kernel and rootfs including agent and crun.
  - will create `_/build/<arch>` and put `root.erofs`, `vmlinux`, and `vmlinux.kconfig` there.

## Building for different architectures

If qemu-user-static is installed on the build host and `binfmt-misc` is set up appropriately, builds for different architectures can be performed.
Note that the build containers run on their _native_ architecture and are _software emulated_ on the host.
This means that these builds will take many times longer than host-native builds.

Pass `--arch <arch>` to the build scripts for a software-emulated build.
* `--arch x86-64` will build for amd64
* `--arch arm64` will build for ARM64
