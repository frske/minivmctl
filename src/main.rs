use scratch_new::vm_manager::{VmConfig, VmManager};
use std::{thread, time::Duration};

const KERNEL_PATH: &str = "/home/frsk/prj/kernel_images/linux-6.6.99/vmlinux";
const SSH_KEY_PATH: &str = "/home/frsk/.ssh/id_ed25519.pub";
const VM_IDS: [u8; 3] = [0, 1, 2];

fn main() {
    let mut manager = VmManager::new();
    let handle = manager.handle().unwrap();

    for vm_id in VM_IDS {
        let config = VmConfig::new(vm_id, KERNEL_PATH, SSH_KEY_PATH).unwrap();
        manager.add_vm(config).unwrap();
    }

    thread::spawn(move || {
        thread::sleep(Duration::from_secs(20));
        println!("Stopping VM 1 after 20 seconds");
        handle.request_stop_vm(1).unwrap();
    });

    manager.run().unwrap();
}
