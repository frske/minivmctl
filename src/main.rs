use std::thread;

use scratch_new::vm_manager::VmManager;
use tracing::info;
use tracing_subscriber::EnvFilter;

const DEFAULT_LOG_FILTER: &str = "scratch_new=info,rocket=warn";

#[rocket::main]
async fn main() -> Result<(), rocket::Error> {
    init_tracing();

    let mut manager = VmManager::new();
    let handle = manager
        .handle()
        .expect("failed to create VM manager handle");

    thread::spawn(move || {
        let _ = manager.run();
    });

    info!("launching api server");
    scratch_new::api::rocket(handle).launch().await?;

    Ok(())
}

fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER));

    tracing_subscriber::fmt().with_env_filter(filter).init();
}
