use ini::Ini;
use nix::mount::umount;
use nix::unistd::Uid;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const ROOTFS_PATH: &str = "/home/frsk/prj/kernel_images/rootfs.ext4";
// const ROOTFS_PATH: &str = "/home/frsk/prj/kernel_images/rootfs.ext4";

pub type ProvisionError = Box<dyn std::error::Error + Send + Sync>;
pub type ProvisionResult<T> = Result<T, ProvisionError>;

fn vm_rootfs_dir(id: u8) -> PathBuf {
    PathBuf::from(format!("/tmp/vm-{}", id))
}

pub fn vm_rootfs_path(id: u8) -> PathBuf {
    vm_rootfs_dir(id).join("rootfs.ext4")
}

pub fn provision_shared_host() -> ProvisionResult<()> {
    if !is_root() {
        return Err("provision_shared_host requires root".into());
    }

    let host_interface = default_interface_name()?;

    ensure_bridge_exists()?;
    configure_iptables(&host_interface)?;

    Ok(())
}

pub fn provision_vm_host(id: u8) -> ProvisionResult<()> {
    if !is_root() {
        return Err("provision_vm_host requires root".into());
    }

    configure_tap_interface(id)
}

pub fn provision_guest(id: u8, public_ssh_key: &str) -> ProvisionResult<()> {
    if !is_root() {
        return Err("provision_guest requires root".into());
    }

    let block_device = create_rootfs(id)?;

    let mount_path = vm_rootfs_dir(id).join("mnt");
    fs::create_dir_all(&mount_path)?;

    mount_rootfs(&block_device, &mount_path)?;

    let net_conf = create_net_config(id);
    provision_guest_net_config(id, net_conf, &mount_path)?;

    provision_ssh(&mount_path, public_ssh_key)?;

    unmount_rootfs(&mount_path)?;

    Ok(())
}

pub fn deprovision_guest(id: u8) -> ProvisionResult<()> {
    let rootfs_dir = vm_rootfs_dir(id);
    fs::remove_dir_all(&rootfs_dir)?;
    Ok(())
}

pub fn deprovision_vm_host(id: u8) -> ProvisionResult<()> {
    if !is_root() {
        return Err("deprovision_vm_host requires root".into());
    }

    let tap_name = format!("tap{}", id);

    let status = Command::new("ip")
        .args(["tuntap", "del", "dev", &tap_name, "mode", "tap"])
        .status()?;

    if !status.success() {
        return Err(format!("Failed deleting {} interface", &tap_name).into());
    }

    Ok(())
}

pub fn deprovision_shared_host() -> ProvisionResult<()> {
    if !is_root() {
        return Err("deprovision_shared_host requires root".into());
    }

    let host_interface = default_interface_name()?;

    deconfigure_iptables(&host_interface)?;
    remove_bridge()?;

    Ok(())
}

fn create_rootfs(id: u8) -> ProvisionResult<PathBuf> {
    let template = PathBuf::from(ROOTFS_PATH);
    let rootfs_dir = vm_rootfs_dir(id);
    fs::create_dir_all(&rootfs_dir)?;

    let new_rootfs = vm_rootfs_path(id);
    fs::copy(&template, &new_rootfs)?;

    fs::set_permissions(&rootfs_dir, fs::Permissions::from_mode(0o755))?;
    fs::set_permissions(&new_rootfs, fs::Permissions::from_mode(0o666))?;

    Ok(new_rootfs)
}

fn ensure_bridge_exists() -> ProvisionResult<()> {
    let status = Command::new("ip")
        .args(["link", "show", "br0"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    match status {
        Ok(s) if s.success() => return Ok(()),
        _ => {}
    }

    println!("Creating bridge br0");

    Command::new("ip")
        .args(["link", "add", "br0", "type", "bridge"])
        .status()?;

    Command::new("ip")
        .args(["addr", "add", "10.10.0.1/24", "dev", "br0"])
        .status()?;

    Command::new("ip")
        .args(["link", "set", "br0", "up"])
        .status()?;

    Command::new("sysctl")
        .args(["-w", "net.ipv4.ip_forward=1"])
        .stdout(std::process::Stdio::null())
        .status()?;

    Ok(())
}

fn configure_tap_interface(id: u8) -> ProvisionResult<()> {
    let tap_name = format!("tap{}", id);

    let exists = Command::new("ip")
        .args(["link", "show", &tap_name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    if matches!(exists, Ok(s) if s.success()) {
        return Ok(());
    }

    Command::new("ip")
        .args(["tuntap", "add", "dev", &tap_name, "mode", "tap"])
        .status()?;

    Command::new("ip")
        .args(["link", "set", "dev", &tap_name, "up"])
        .status()?;

    Command::new("ip")
        .args(["link", "set", &tap_name, "master", "br0"])
        .status()?;

    Ok(())
}

fn configure_iptables(interface_name: &str) -> ProvisionResult<()> {
    let ipt = iptables::new(false).map_err(|e| e.to_string())?;

    for (table, chain, rule) in iptables_rules(interface_name) {
        if !ipt.exists(table, chain, &rule).map_err(|e| e.to_string())? {
            ipt.append(table, chain, &rule).map_err(|e| e.to_string())?;
        }
    }

    Ok(())
}

fn deconfigure_iptables(interface_name: &str) -> ProvisionResult<()> {
    let ipt = iptables::new(false).map_err(|e| e.to_string())?;

    for (table, chain, rule) in iptables_rules(interface_name) {
        if ipt.exists(table, chain, &rule).map_err(|e| e.to_string())? {
            ipt.delete(table, chain, &rule).map_err(|e| e.to_string())?;
        }
    }

    Ok(())
}

fn iptables_rules(interface_name: &str) -> [(&'static str, &'static str, String); 3] {
    [
        (
            "nat",
            "POSTROUTING",
            format!("-s 10.10.0.0/24 -o {} -j MASQUERADE", interface_name),
        ),
        (
            "filter",
            "FORWARD",
            format!("-i br0 -o {} -j ACCEPT", interface_name),
        ),
        (
            "filter",
            "FORWARD",
            format!(
                "-i {} -o br0 -m state --state RELATED,ESTABLISHED -j ACCEPT",
                interface_name
            ),
        ),
    ]
}

fn remove_bridge() -> ProvisionResult<()> {
    let bridge_exists = Command::new("ip")
        .args(["link", "show", "br0"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    if !matches!(bridge_exists, Ok(status) if status.success()) {
        return Ok(());
    }

    let status = Command::new("ip").args(["link", "del", "br0"]).status()?;

    if !status.success() {
        return Err("failed deleting br0 interface".into());
    }

    Ok(())
}

fn create_net_config(id: u8) -> Ini {
    println!("Creating network config file");
    let ip_address = format!("10.10.0.{}/24", id + 2);
    let gateway = "10.10.0.1";
    let mut conf = Ini::new();

    conf.with_section(Some("Match")).set("Name", "eth0");

    conf.with_section(Some("Network"))
        .set("Address", &ip_address)
        .set("Gateway", gateway)
        .set("DNS", "8.8.8.8");

    conf
}

fn provision_guest_net_config(id: u8, conf: Ini, mount_path: &Path) -> ProvisionResult<()> {
    let conf_path = mount_path
        .join("etc")
        .join("systemd")
        .join("network")
        .join(format!("{}-eth.network", id));

    println!("Writing network config file");

    conf.write_to_file(&conf_path)?;

    Ok(())
}

fn mount_rootfs(block_device_path: &Path, mount_path: &Path) -> ProvisionResult<()> {
    println!("Mounting block device");

    let device_str = block_device_path
        .to_str()
        .ok_or_else(|| format!("invalid device path: {:?}", block_device_path))?;
    let mount_str = mount_path
        .to_str()
        .ok_or_else(|| format!("invalid mount path: {:?}", mount_path))?;

    Command::new("mount")
        .args([device_str, mount_str])
        .status()?;

    Ok(())
}

fn unmount_rootfs(mount_path: &Path) -> ProvisionResult<()> {
    println!("Unmounting block device");

    umount(mount_path)?;
    let _ = fs::remove_dir_all(mount_path);

    Ok(())
}

fn provision_ssh(mount_path: &Path, public_key: &str) -> ProvisionResult<()> {
    let ssh_dir = mount_path.join("root").join(".ssh");
    fs::create_dir_all(&ssh_dir)?;
    let authorized_keys_file = ssh_dir.join("authorized_keys");
    fs::write(authorized_keys_file, public_key)?;

    Ok(())
}

fn is_root() -> bool {
    Uid::effective().is_root()
}

fn default_interface_name() -> ProvisionResult<String> {
    let active_interface = netdev::get_default_interface().map_err(|e| e.to_string())?;
    Ok(active_interface.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn vm_dir(id: u8) -> PathBuf {
        PathBuf::from(format!("/tmp/vm-{}", id))
    }

    fn allocate_vm_id() -> u8 {
        for id in 1..=250 {
            if !vm_dir(id).exists() {
                return id;
            }
        }

        panic!("no free vm id available for test");
    }

    #[test]
    fn vm_rootfs_path_points_into_vm_directory() {
        assert_eq!(vm_rootfs_path(7), PathBuf::from("/tmp/vm-7/rootfs.ext4"));
    }

    #[test]
    fn provision_guest_net_config_writes_expected_network_file() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let id = allocate_vm_id();
        let mount_path = vm_dir(id).join("mnt");
        let conf_path = mount_path.join("etc").join("systemd").join("network");

        let _ = fs::remove_dir_all(vm_dir(id));
        fs::create_dir_all(&conf_path).unwrap();

        provision_guest_net_config(id, create_net_config(id), &mount_path).unwrap();

        let contents = fs::read_to_string(conf_path.join(format!("{}-eth.network", id))).unwrap();
        assert!(contents.contains("[Match]"));
        assert!(contents.contains("Name=eth0"));
        assert!(contents.contains("[Network]"));
        assert!(
            contents.contains(&format!("Address=10.10.0.{}/24", id + 2)),
            "unexpected address contents: {contents}"
        );
        assert!(contents.contains("Gateway=10.10.0.1"));
        assert!(contents.contains("DNS=8.8.8.8"));

        let _ = fs::remove_dir_all(vm_dir(id));
    }

    #[test]
    fn provision_ssh_writes_authorized_keys() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let id = allocate_vm_id();
        let mount_path = vm_dir(id).join("mnt");
        let ssh_dir = mount_path.join("root").join(".ssh");

        let _ = fs::remove_dir_all(vm_dir(id));
        fs::create_dir_all(&mount_path).unwrap();

        provision_ssh(&mount_path, "ssh-ed25519 AAAA test@example").unwrap();

        assert_eq!(
            fs::read_to_string(ssh_dir.join("authorized_keys")).unwrap(),
            "ssh-ed25519 AAAA test@example"
        );

        let _ = fs::remove_dir_all(vm_dir(id));
    }

    #[test]
    fn deprovision_guest_removes_vm_directory() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let id = allocate_vm_id();
        let root = vm_dir(id);

        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("mnt")).unwrap();
        fs::write(root.join("marker"), b"test").unwrap();

        deprovision_guest(id).unwrap();

        assert!(!root.exists());
    }
}
