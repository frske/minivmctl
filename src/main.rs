use std::{sync::mpsc, thread, time::Duration};

use scratch_new::vm_manager::VmManager;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

const DEFAULT_LOG_FILTER: &str = "scratch_new=info,rocket=warn";
const MANAGER_START_TIMEOUT: Duration = Duration::from_secs(5);
const MANAGER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

#[rocket::main]
async fn main() -> Result<(), Box<rocket::Error>> {
    init_tracing();

    let mut manager = VmManager::new();
    let handle = manager
        .handle()
        .expect("failed to create VM manager handle");

    let shutdown_handle = handle.clone();
    let (manager_done_tx, manager_done_rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = manager_done_tx.send(manager.run());
    });

    if let Err(err) = handle.wait_until_running(MANAGER_START_TIMEOUT) {
        warn!(error = %err, "vm manager did not report ready before api launch");
    }

    info!("launching api server");
    let launch_result = scratch_new::api::rocket(handle).launch().await;

    info!("api server stopped, shutting down vm manager");
    if let Err(err) = shutdown_handle.shutdown() {
        warn!(error = %err, "failed to request vm manager shutdown");
    }

    match manager_done_rx.recv_timeout(MANAGER_SHUTDOWN_TIMEOUT) {
        Ok(Ok(())) => info!("vm manager stopped"),
        Ok(Err(err)) => error!(error = %err, "vm manager stopped with error"),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            error!("vm manager did not stop before shutdown timeout")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => error!("vm manager thread panicked"),
    }

    launch_result.map(|_| ()).map_err(Box::new)
}

fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER));

    tracing_subscriber::fmt().with_env_filter(filter).init();
}
