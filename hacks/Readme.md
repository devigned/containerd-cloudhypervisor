# Containerised build hack scripts

Scripts are meant to be called from the repository root, and should be invoked with `bash` (for example, `bash hacks/build-guest-kernel.sh`).

* `hacks/build-guest-kernel.sh` - build the guest (L2) kernel in an ephemeral Alpine container.
  - will put `vmlinux` and `vmlinux.kconfig` in the repo root after build.
* `hacks/build-static-rust.sh` - statically compile Rust binaries from this repo.
  - e.g. `bash hacks/build-static-rust.sh containerd-shim-cloudhv crates/agent/cloudhv-agent` will build both L1 containerd shim and L2 cloud hypervisor agent.
  - will put the build result (static binary) into the repo root.
* `hacks/build-guest.sh` - Build a full guest, kernel and rootfs including agent and crun.
  - will  put `rootfs.erofs`, `vmlinux`, and `vmlinux.kconfig` into the repository root.
* `hacks/build-host-deps.sh` - Build host (L1) dependencies. All deps are statically linked. Currently only `mkfs.erofs` is built.
  - will put static `mkfs.erofs` into the repo root.
* `hacks/build-sysext.sh` - Will build a system extension image with guest (L2) and host bits, including host configuration.
  - will put `containerd-cloudhypervisor.raw` into the repo root.
    See [main readme](../README.md#test-your-builds-locally-in-a-flatcar-vm) for usage instructions.
* `hacks/test-sysext.sh` - Will boot a Flatcar VM with the sysext in the repo root, run `/usr/share/cloudhv/demo/demo.sh`, and exit with the demo's exit code.
  - needs `containerd-cloudhypervisor.raw` present in the repo root.
  - will clone the [sysext bakery](https://github.com/flatcar/sysext-bakery/) repo locally and use `bakery.sh boot` to run the test.

## Building for different architectures

If qemu-user-static is installed on the build host and `binfmt-misc` is set up appropriately, builds for different architectures can be performed.
Note that the build containers run on their _native_ architecture and are _software emulated_ on the host.
This means that these builds will take many times longer than host-native builds.

Pass `--arch <arch>` to the build scripts for a software-emulated build.
* `--arch x86-64` will build for amd64
* `--arch arm64` will build for ARM64
