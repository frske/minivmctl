use std::thread;

use scratch_new::vm_manager::VmManager;

#[rocket::main]
async fn main() -> Result<(), rocket::Error> {
    let mut manager = VmManager::new();
    let handle = manager
        .handle()
        .expect("failed to create VM manager handle");

    thread::spawn(move || {
        manager.run().expect("VM manager failed");
    });

    scratch_new::api::rocket(handle).launch().await?;

    Ok(())
}
