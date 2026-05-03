use scratch_new::vm_manager::{VmConfig, VmManager};

const KERNEL_PATH: &str = "/home/frsk/prj/kernel_images/linux-6.6.99/vmlinux";
const SSH_KEY_PATH: &str = "/home/frsk/.ssh/id_ed25519.pub";
const VM_IDS: [u8; 3] = [0, 1, 2];

fn main() {
    let mut manager = VmManager::new();

    for vm_id in VM_IDS {
        let config = VmConfig::new(vm_id, KERNEL_PATH, SSH_KEY_PATH).unwrap();
        manager.add_vm(config).unwrap();
    }

    manager.run().unwrap();
}
