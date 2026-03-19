use containerd_shimkit::sandbox;

mod annotations;
mod config;
mod hypervisor;
mod instance;
mod memory;
mod netns;
mod vm;
mod vsock;

use instance::CloudHvInstance;

fn main() {
    sandbox::cli::shim_main::<CloudHvInstance>(
        "io.containerd.cloudhv.v1",
        sandbox::cli::Version {
            version: env!("CARGO_PKG_VERSION"),
            revision: "dev",
        },
        None,
    );
}
