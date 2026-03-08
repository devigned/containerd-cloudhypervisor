use containerd_shim::asynchronous::run;
use containerd_shim::Config;
use log::info;

mod config;
mod hypervisor;
mod image_cache;
mod instance;
mod pool;
mod vm;
mod vsock;

use instance::CloudHvShim;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    info!(
        "containerd-shim-cloudhv-v1 starting (version {})",
        env!("CARGO_PKG_VERSION")
    );

    let backend = hypervisor::detect_hypervisor();
    info!("hypervisor backend: {}", backend);

    let config = Config {
        no_reaper: false,
        no_sub_reaper: false,
        ..Default::default()
    };

    run::<CloudHvShim>("io.containerd.cloudhv.v1", Some(config)).await;
}
