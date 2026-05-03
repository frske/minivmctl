use std::{
    collections::HashMap,
    fs,
    fs::OpenOptions,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use kvm_ioctls::{IoEventAddress, VcpuExit, VmFd};
use vm_superio::{Serial, Trigger};
use vmm_sys_util::{eventfd::EventFd, poll::{PollContext, PollEvents}};

use crate::{
    blk::Blk,
    boot::{self, BootArtifacts},
    bus::MMIOBus,
    event::EventWrapper,
    net::{self, Net},
    provision,
    serial::DummySerialEvent,
    virtio::VirtioDevice,
};

type HostSerial = Serial<EventWrapper, DummySerialEvent, std::io::Sink>;
pub type VmManagerResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const BLK_MMIO_MEM_START: u64 = MMIO_MEM_START + 0x1000;
const COM1: u16 = 0x3f8;
const MMIO_MEM_START: u64 = 0xd0000000;
const QUEUE_NOTIFY_OFFSET: u64 = 0x50;
const MAX_VM_ID: u8 = 252;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EventKind {
    Exit,
    NetActivate,
    NetQueue(usize),
    BlkQueue(usize),
    TapRx,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EventRoute {
    vm_id: u8,
    kind: EventKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InstanceAction {
    Continue,
    Exit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmConfig {
    id: u8,
    kernel_path: PathBuf,
    ssh_key_path: PathBuf,
}

impl VmConfig {
    pub fn new(
        id: u8,
        kernel_path: impl Into<PathBuf>,
        ssh_key_path: impl Into<PathBuf>,
    ) -> VmManagerResult<Self> {
        if id > MAX_VM_ID {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("vm id must be <= {MAX_VM_ID}"),
            )
            .into());
        }

        Ok(Self {
            id,
            kernel_path: kernel_path.into(),
            ssh_key_path: ssh_key_path.into(),
        })
    }

    pub fn id(&self) -> u8 {
        self.id
    }

    pub fn tap_name(&self) -> String {
        format!("tap{}", self.id)
    }

    pub fn guest_ip(&self) -> String {
        format!("10.10.0.{}", self.id + 2)
    }

    pub fn guest_mac(&self) -> [u8; 6] {
        [0x02, 0x12, 0x34, 0x56, 0x78, self.id]
    }

    pub fn rootfs_path(&self) -> PathBuf {
        provision::vm_rootfs_path(self.id)
    }
}

pub struct VmManager {
    configs: HashMap<u8, VmConfig>,
    instances: HashMap<u8, VmInstance>,
    shared_host_provisioned: bool,
}

impl VmManager {
    pub fn new() -> Self {
        Self {
            configs: HashMap::new(),
            instances: HashMap::new(),
            shared_host_provisioned: false,
        }
    }

    pub fn add_vm(&mut self, config: VmConfig) -> VmManagerResult<()> {
        if self.configs.contains_key(&config.id()) || self.instances.contains_key(&config.id()) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("duplicate vm id {}", config.id()),
            )
            .into());
        }

        self.configs.insert(config.id(), config);
        Ok(())
    }

    pub fn run(&mut self) -> VmManagerResult<()> {
        let result = self.run_inner();
        self.cleanup_instances();
        self.cleanup_shared_host();
        result
    }

    fn run_inner(&mut self) -> VmManagerResult<()> {
        if self.instances.is_empty() && self.configs.is_empty() {
            return Err(io::Error::other("vm manager requires at least one configured vm").into());
        }

        if !self.shared_host_provisioned {
            provision::provision_shared_host()?;
            self.shared_host_provisioned = true;
        }

        if self.instances.is_empty() {
            self.start_configured_vms()?;
        }

        let poll: PollContext<u8> = PollContext::new().unwrap();
        let mut routes = HashMap::new();
        let mut next_token_count = 0usize;

        self.register_instance_events(&poll, &mut routes, &mut next_token_count)?;

        while !self.instances.is_empty() {
            let events: PollEvents<u8> = poll.wait().unwrap();
            for event in events.iter_readable() {
                let Some(route) = routes.get(&event.token()).copied() else {
                    continue;
                };

                match self.dispatch_event(route, &poll, &mut routes, &mut next_token_count)? {
                    InstanceAction::Continue => {}
                    InstanceAction::Exit => self.remove_instance(route.vm_id, &mut routes),
                }
            }
        }

        Ok(())
    }

    fn start_configured_vms(&mut self) -> VmManagerResult<()> {
        let configs = std::mem::take(&mut self.configs);

        for (vm_id, config) in configs {
            let instance = VmInstance::start(config)?;
            self.instances.insert(vm_id, instance);
        }

        Ok(())
    }

    fn register_instance_events(
        &mut self,
        poll: &PollContext<u8>,
        routes: &mut HashMap<u8, EventRoute>,
        next_token_count: &mut usize,
    ) -> VmManagerResult<()> {
        let mut vm_ids = self.instances.keys().copied().collect::<Vec<_>>();
        vm_ids.sort_unstable();

        for vm_id in vm_ids {
            let instance = self.instances.get(&vm_id).unwrap();
            instance.register_base_events(vm_id, poll, routes, next_token_count)?;
        }

        Ok(())
    }

    fn dispatch_event(
        &mut self,
        route: EventRoute,
        poll: &PollContext<u8>,
        routes: &mut HashMap<u8, EventRoute>,
        next_token_count: &mut usize,
    ) -> VmManagerResult<InstanceAction> {
        let Some(instance) = self.instances.get_mut(&route.vm_id) else {
            return Ok(InstanceAction::Continue);
        };

        match route.kind {
            EventKind::Exit => Ok(instance.handle_exit()),
            EventKind::NetActivate => {
                instance.handle_net_activate(route.vm_id, poll, routes, next_token_count)
            }
            EventKind::TapRx => instance.handle_tap_rx(),
            EventKind::NetQueue(idx) => instance.handle_net_queue(idx),
            EventKind::BlkQueue(idx) => instance.handle_blk_queue(idx),
        }
    }

    fn remove_instance(&mut self, vm_id: u8, routes: &mut HashMap<u8, EventRoute>) {
        routes.retain(|_, route| route.vm_id != vm_id);
        self.instances.remove(&vm_id);
        let _ = provision::deprovision_guest(vm_id);
        let _ = provision::deprovision_vm_host(vm_id);
    }

    fn cleanup_instances(&mut self) {
        for vm_id in self.instances.keys().copied().collect::<Vec<_>>() {
            let _ = provision::deprovision_guest(vm_id);
            let _ = provision::deprovision_vm_host(vm_id);
        }

        self.instances.clear();
    }

    fn cleanup_shared_host(&mut self) {
        if self.shared_host_provisioned {
            let _ = provision::deprovision_shared_host();
            self.shared_host_provisioned = false;
        }
    }
}

impl Drop for VmManager {
    fn drop(&mut self) {
        self.cleanup_instances();
        self.cleanup_shared_host();
    }
}

struct VmInstance {
    id: u8,
    _vm: VmFd,
    virtio_net_activate_evt: EventFd,
    virtio_net_queue_events: Vec<EventFd>,
    virtio_net: Arc<Mutex<Net>>,
    virtio_blk_queue_events: Vec<EventFd>,
    virtio_blk: Arc<Mutex<Blk>>,
    exit_evt: EventWrapper,
    tap_rx_registered: bool,
}

impl VmInstance {
    fn start(config: VmConfig) -> VmManagerResult<Self> {
        provision::provision_vm_host(config.id()).expect("failed to provision host");
        let public_ssh_key = fs::read_to_string(&config.ssh_key_path).expect("failed to read SSH key");
        provision::provision_guest(config.id(), public_ssh_key.trim_end())
            .expect("failed to provision guest");

        println!(
            "Provisioned VM {}\n  rootfs: {}\n  tap: {}\n  guest ip: {}\n  gateway: 10.10.0.1\n  dns: 8.8.8.8",
            config.id(),
            config.rootfs_path().display(),
            config.tap_name(),
            config.guest_ip(),
        );

        let BootArtifacts {
            vm,
            vcpu,
            guest_mem,
        } = boot::setup(path_to_str(&config.kernel_path)?);

        let mut mmio_bus = MMIOBus::new();

        let virtio_net = net::Net::new(&config.tap_name(), Some(config.guest_mac()))
            .expect("failed to create virtio net");
        vm.register_irqfd(virtio_net.interrupt_event(), 5).unwrap();
        for (q_sel, evt) in virtio_net.queues_events().iter().enumerate() {
            let addr = IoEventAddress::Mmio(QUEUE_NOTIFY_OFFSET + MMIO_MEM_START);
            vm.register_ioevent(evt, &addr, q_sel as u32).unwrap();
        }

        let virtio_net_activate_evt = virtio_net.activate_event.try_clone().unwrap();
        let virtio_net_queue_events = virtio_net
            .queues_events()
            .iter()
            .map(|evt| evt.try_clone().unwrap())
            .collect::<Vec<_>>();
        let virtio_net = Arc::new(Mutex::new(virtio_net));
        mmio_bus
            .insert_virtio(MMIO_MEM_START, virtio_net.clone(), guest_mem.clone())
            .expect("insert virtio net mmio");

        let virtio_blk = Blk::new(
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(config.rootfs_path())
                .expect("failed to open block path"),
        )
        .expect("failed to create virtio blk");
        vm.register_irqfd(virtio_blk.interrupt_event(), 6).unwrap();
        for (q_sel, evt) in virtio_blk.queues_events().iter().enumerate() {
            let addr = IoEventAddress::Mmio(QUEUE_NOTIFY_OFFSET + BLK_MMIO_MEM_START);
            vm.register_ioevent(evt, &addr, q_sel as u32).unwrap();
        }
        let virtio_blk_queue_events = virtio_blk
            .queues_events()
            .iter()
            .map(|evt| evt.try_clone().unwrap())
            .collect::<Vec<_>>();
        let virtio_blk = Arc::new(Mutex::new(virtio_blk));
        mmio_bus
            .insert_virtio(BLK_MMIO_MEM_START, virtio_blk.clone(), guest_mem.clone())
            .expect("insert virtio blk mmio");

        let com_evt_1 = EventWrapper::new();
        vm.register_irqfd(&com_evt_1.0, 4).unwrap();

        let stdio_serial = Arc::new(Mutex::new(Serial::with_events(
            com_evt_1.try_clone().unwrap(),
            DummySerialEvent,
            std::io::sink(),
        )));

        let exit_evt = EventWrapper::new();
        spawn_vcpu_runtime(
            vcpu,
            stdio_serial,
            mmio_bus,
            exit_evt.try_clone().unwrap(),
        );

        Ok(Self {
            id: config.id(),
            _vm: vm,
            virtio_net_activate_evt,
            virtio_net_queue_events,
            virtio_net,
            virtio_blk_queue_events,
            virtio_blk,
            exit_evt,
            tap_rx_registered: false,
        })
    }

    fn register_base_events(
        &self,
        vm_id: u8,
        poll: &PollContext<u8>,
        routes: &mut HashMap<u8, EventRoute>,
        next_token_count: &mut usize,
    ) -> VmManagerResult<()> {
        register_event(
            poll,
            &self.exit_evt.0,
            EventRoute {
                vm_id,
                kind: EventKind::Exit,
            },
            routes,
            next_token_count,
        )?;
        register_event(
            poll,
            &self.virtio_net_activate_evt,
            EventRoute {
                vm_id,
                kind: EventKind::NetActivate,
            },
            routes,
            next_token_count,
        )?;
        for (q_sel, evt) in self.virtio_net_queue_events.iter().enumerate() {
            register_event(
                poll,
                evt,
                EventRoute {
                    vm_id,
                    kind: EventKind::NetQueue(q_sel),
                },
                routes,
                next_token_count,
            )?;
        }
        for (q_sel, evt) in self.virtio_blk_queue_events.iter().enumerate() {
            register_event(
                poll,
                evt,
                EventRoute {
                    vm_id,
                    kind: EventKind::BlkQueue(q_sel),
                },
                routes,
                next_token_count,
            )?;
        }

        Ok(())
    }

    fn handle_exit(&self) -> InstanceAction {
        println!("vcpu stopped, main loop exit for vm {}", self.id);
        InstanceAction::Exit
    }

    fn handle_net_activate(
        &mut self,
        vm_id: u8,
        poll: &PollContext<u8>,
        routes: &mut HashMap<u8, EventRoute>,
        next_token_count: &mut usize,
    ) -> VmManagerResult<InstanceAction> {
        let _ = self.virtio_net_activate_evt.read().unwrap();
        if !self.tap_rx_registered {
            let token = next_token(next_token_count)?;
            self.virtio_net
                .lock()
                .unwrap()
                .register_tap_readable(poll, token)
                .unwrap();
            routes.insert(
                token,
                EventRoute {
                    vm_id,
                    kind: EventKind::TapRx,
                },
            );
            self.tap_rx_registered = true;
        }

        Ok(InstanceAction::Continue)
    }

    fn handle_tap_rx(&mut self) -> VmManagerResult<InstanceAction> {
        let mut net = self.virtio_net.lock().unwrap();
        let _ = net.process_rx();
        Ok(InstanceAction::Continue)
    }

    fn handle_net_queue(&mut self, idx: usize) -> VmManagerResult<InstanceAction> {
        let _ = self.virtio_net_queue_events[idx].read().unwrap();
        let mut net = self.virtio_net.lock().unwrap();
        let _ = net.process_tx();
        Ok(InstanceAction::Continue)
    }

    fn handle_blk_queue(&mut self, idx: usize) -> VmManagerResult<InstanceAction> {
        let _ = self.virtio_blk_queue_events[idx].read().unwrap();
        let mut blk = self.virtio_blk.lock().unwrap();
        let _ = blk.process_queue();
        Ok(InstanceAction::Continue)
    }
}

fn spawn_vcpu_runtime(
    mut vcpu: kvm_ioctls::VcpuFd,
    stdio_serial: Arc<Mutex<HostSerial>>,
    mut mmio_bus: MMIOBus,
    vcpu_exit_evt: EventWrapper,
) {
    std::thread::spawn(move || {
        loop {
            match vcpu.run() {
                Ok(run) => match run {
                    VcpuExit::IoIn(addr, data) => {
                        if addr >= COM1 && addr - COM1 < 8 {
                            data[0] = lock_host_serial(&stdio_serial).read((addr - COM1) as u8)
                        }
                    }
                    VcpuExit::IoOut(addr, data) => {
                        if addr >= COM1 && addr - COM1 < 8 {
                            let _ = lock_host_serial(&stdio_serial)
                                .write((addr - COM1) as u8, data[0]);
                        }
                    }
                    VcpuExit::MmioRead(addr, data) => {
                        mmio_bus.read(addr, data);
                    }
                    VcpuExit::MmioWrite(addr, data) => {
                        mmio_bus.write(addr, data);
                    }
                    VcpuExit::Hlt => {
                        println!("KVM_EXIT_HLT");
                        break;
                    }
                    VcpuExit::Shutdown => {
                        println!("KVM_EXIT_SHUTDOWN");
                        break;
                    }
                    r => {
                        println!("KVM_EXIT: {:?}", r)
                    }
                },
                Err(e) => {
                    println!("KVM Run error {:?}", e);
                    break;
                }
            }
        }
        vcpu_exit_evt.trigger().unwrap();
    });
}

fn lock_host_serial(serial: &Arc<Mutex<HostSerial>>) -> std::sync::MutexGuard<'_, HostSerial> {
    match serial.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn path_to_str(path: &Path) -> VmManagerResult<&str> {
    path.to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path is not valid UTF-8").into())
}

fn register_event(
    poll: &PollContext<u8>,
    fd: &impl std::os::fd::AsRawFd,
    route: EventRoute,
    routes: &mut HashMap<u8, EventRoute>,
    next_token_count: &mut usize,
) -> VmManagerResult<()> {
    let token = next_token(next_token_count)?;
    poll.add(fd, token)
        .map_err(|e| io::Error::from_raw_os_error(e.errno()))?;
    routes.insert(token, route);
    Ok(())
}

fn next_token(next_token_count: &mut usize) -> VmManagerResult<u8> {
    let token = u8::try_from(*next_token_count)
        .map_err(|_| io::Error::other("exhausted shared u8 event tokens"))?;
    *next_token_count += 1;
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, io::ErrorKind};

    #[test]
    fn vm_config_derives_identity_from_id() {
        let config = VmConfig::new(7, "/kernel", "/ssh").unwrap();

        assert_eq!(config.tap_name(), "tap7");
        assert_eq!(config.guest_ip(), "10.10.0.9");
        assert_eq!(config.guest_mac(), [0x02, 0x12, 0x34, 0x56, 0x78, 7]);
        assert_eq!(config.rootfs_path(), PathBuf::from("/tmp/vm-7/rootfs.ext4"));
    }

    #[test]
    fn vm_config_rejects_invalid_id_range() {
        assert!(VmConfig::new(253, "/kernel", "/ssh").is_err());
    }

    #[test]
    fn add_vm_rejects_duplicate_ids() {
        let mut manager = VmManager::new();
        let config = VmConfig::new(1, "/kernel", "/ssh").unwrap();

        manager.add_vm(config.clone()).unwrap();

        let err = manager.add_vm(config).unwrap_err();
        assert_eq!(err.downcast_ref::<io::Error>().unwrap().kind(), ErrorKind::AlreadyExists);
    }

    // No vm in bag. Manager say no.
    #[test]
    fn run_rejects_empty_manager() {
        let mut manager = VmManager::new();

        let err = manager.run().unwrap_err();

        assert_eq!(err.downcast_ref::<io::Error>().unwrap().kind(), ErrorKind::Other);
    }

    // Token count go up to 255, then no more token.
    #[test]
    fn next_token_uses_full_u8_range_then_exhausts() {
        let mut next_token_count = 254;

        assert_eq!(next_token(&mut next_token_count).unwrap(), 254);
        assert_eq!(next_token(&mut next_token_count).unwrap(), 255);
        assert_eq!(next_token_count, 256);

        let err = next_token(&mut next_token_count).unwrap_err();
        assert_eq!(err.downcast_ref::<io::Error>().unwrap().kind(), ErrorKind::Other);
        assert_eq!(next_token_count, 256);
    }

    // New token go in map with right route.
    #[test]
    fn register_event_records_route_under_allocated_token() {
        let poll: PollContext<u8> = PollContext::new().unwrap();
        let evt = EventWrapper::new();
        let mut routes = HashMap::new();
        let mut next_token_count = 0;
        let route = EventRoute {
            vm_id: 9,
            kind: EventKind::NetQueue(1),
        };

        register_event(&poll, &evt.0, route, &mut routes, &mut next_token_count).unwrap();

        assert_eq!(next_token_count, 1);
        assert_eq!(routes.get(&0), Some(&route));
    }

    // Remove one vm route. Other vm route stay.
    #[test]
    fn remove_instance_prunes_only_target_vm_routes() {
        let mut manager = VmManager::new();
        let mut routes = HashMap::from([
            (
                1,
                EventRoute {
                    vm_id: 3,
                    kind: EventKind::Exit,
                },
            ),
            (
                2,
                EventRoute {
                    vm_id: 4,
                    kind: EventKind::TapRx,
                },
            ),
            (
                3,
                EventRoute {
                    vm_id: 3,
                    kind: EventKind::NetActivate,
                },
            ),
        ]);

        manager.remove_instance(3, &mut routes);

        assert_eq!(routes.len(), 1);
        assert_eq!(
            routes.get(&2),
            Some(&EventRoute {
                vm_id: 4,
                kind: EventKind::TapRx,
            })
        );
    }

    // Route point to no vm. Manager do nothing.
    #[test]
    fn dispatch_event_ignores_missing_instance_route() {
        let mut manager = VmManager::new();
        let poll: PollContext<u8> = PollContext::new().unwrap();
        let mut routes = HashMap::new();
        let mut next_token_count = 0;

        let action = manager
            .dispatch_event(
                EventRoute {
                    vm_id: 11,
                    kind: EventKind::Exit,
                },
                &poll,
                &mut routes,
                &mut next_token_count,
            )
            .unwrap();

        assert_eq!(action, InstanceAction::Continue);
    }
}
