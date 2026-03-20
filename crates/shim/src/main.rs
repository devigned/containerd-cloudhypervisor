#![deny(dead_code)]

use containerd_shimkit::sandbox;

mod annotations;
mod config;
mod instance;
mod memory;
mod netns;
mod snapshot;
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
