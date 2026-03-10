use containerd_shim::asynchronous::run;
use containerd_shim::Config;

mod config;
mod hypervisor;
mod image_cache;
mod instance;
mod pool;
mod vm;
mod vsock;

use instance::CloudHvShim;

#[tokio::main]
async fn main() {
    // IMPORTANT: Do NOT log anything before run() returns.
    // During the "start" action, containerd reads the shim's stdout+stderr
    // to get the TTRPC socket address. Any output corrupts the address.
    let config = Config {
        no_reaper: false,
        no_sub_reaper: false,
        ..Default::default()
    };

    run::<CloudHvShim>("io.containerd.cloudhv.v1", Some(config)).await;
}
