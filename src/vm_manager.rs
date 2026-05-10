use std::{
    collections::HashMap,
    fs::OpenOptions,
    io,
    path::PathBuf,
    ptr,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicPtr, Ordering},
        mpsc,
    },
    time::Duration,
};

use kvm_ioctls::{IoEventAddress, VcpuExit, VmFd};
use tracing::{debug, error, info, warn};
use vm_memory::GuestMemoryMmap;
use vm_superio::{Serial, Trigger};
use vmm_sys_util::{
    eventfd::EventFd,
    poll::{PollContext, PollEvents},
};

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

const MMIO_MEM_START: u64 = 0xd0000000;
const BLK_MMIO_MEM_START: u64 = MMIO_MEM_START + 0x1000;
const QUEUE_NOTIFY_OFFSET: u64 = 0x50;
const COM1: u16 = 0x3f8;
const COMMAND_TOKEN: u8 = 0;
const KERNEL_PATH: &str = "/home/frsk/prj/kernel_images/linux-6.6.99/vmlinux";
const VM_STOP_SIGNAL: libc::c_int = libc::SIGUSR1;
static STOP_SIGNAL_HANDLER_RESULT: OnceLock<Result<(), i32>> = OnceLock::new();

enum VmCommand {
    Start {
        name: String,
        ssh_key: String,
        response: mpsc::Sender<VmManagerResult<VmInfo>>,
    },
    List {
        response: mpsc::Sender<VmManagerResult<Vec<VmInfo>>>,
    },
    Stop {
        id: u8,
        response: mpsc::Sender<VmManagerResult<()>>,
    },
}

pub struct VmManagerHandle {
    command_tx: mpsc::Sender<VmCommand>,
    command_evt: Arc<EventWrapper>,
    manager_running: Arc<AtomicBool>,
}

impl Clone for VmManagerHandle {
    fn clone(&self) -> Self {
        Self {
            command_tx: self.command_tx.clone(),
            command_evt: Arc::clone(&self.command_evt),
            manager_running: Arc::clone(&self.manager_running),
        }
    }
}

impl VmManagerHandle {
    pub fn start_vm(&self, name: String, ssh_key: String) -> VmManagerResult<VmInfo> {
        self.send_command(
            "vm manager is not running",
            Duration::from_secs(120),
            |response| VmCommand::Start {
                name,
                ssh_key,
                response,
            },
        )
    }

    pub fn list_vms(&self) -> VmManagerResult<Vec<VmInfo>> {
        self.send_command(
            "vm manager is not running",
            Duration::from_secs(5),
            |response| VmCommand::List { response },
        )
    }

    /// Requests a hard stop and returns when the manager accepts the request.
    /// VM cleanup completes later through the existing vCPU exit event path.
    pub fn request_stop_vm(&self, id: u8) -> VmManagerResult<()> {
        self.send_command(
            "vm manager is not running",
            Duration::from_secs(5),
            |response| VmCommand::Stop { id, response },
        )
    }

    fn send_command<T>(
        &self,
        not_running_message: &'static str,
        timeout: Duration,
        command: impl FnOnce(mpsc::Sender<VmManagerResult<T>>) -> VmCommand,
    ) -> VmManagerResult<T> {
        if !self.manager_running.load(Ordering::SeqCst) {
            return Err(io::Error::new(io::ErrorKind::NotConnected, not_running_message).into());
        }

        let (response_tx, response_rx) = mpsc::channel();
        self.command_tx
            .send(command(response_tx))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "vm manager is not running"))?;
        self.command_evt.as_ref().trigger()?;
        response_rx.recv_timeout(timeout).map_err(|err| {
            let kind = match err {
                mpsc::RecvTimeoutError::Timeout => io::ErrorKind::TimedOut,
                mpsc::RecvTimeoutError::Disconnected => io::ErrorKind::BrokenPipe,
            };
            io::Error::new(kind, "vm manager did not process command")
        })?
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmInfo {
    pub id: u8,
    pub name: String,
    pub ip_address: String,
    pub status: &'static str,
}

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmConfig {
    id: u8,
    name: String,
    ssh_key: String,
}

impl VmConfig {
    pub fn id(&self) -> u8 {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn ssh_key(&self) -> &str {
        &self.ssh_key
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
    command_tx: mpsc::Sender<VmCommand>,
    command_rx: mpsc::Receiver<VmCommand>,
    command_evt: Arc<EventWrapper>,
    manager_running: Arc<AtomicBool>,
    next_vm_id: u8,
}

impl VmManager {
    pub fn new() -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        Self {
            configs: HashMap::new(),
            instances: HashMap::new(),
            shared_host_provisioned: false,
            command_tx,
            command_rx,
            command_evt: Arc::new(EventWrapper::new()),
            manager_running: Arc::new(AtomicBool::new(false)),
            next_vm_id: 0,
        }
    }

    pub fn handle(&self) -> VmManagerResult<VmManagerHandle> {
        Ok(VmManagerHandle {
            command_tx: self.command_tx.clone(),
            command_evt: Arc::clone(&self.command_evt),
            manager_running: Arc::clone(&self.manager_running),
        })
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
        info!("vm manager starting");
        self.manager_running.store(true, Ordering::SeqCst);
        let result = self.run_inner();
        if let Err(err) = &result {
            error!(error = %err, "vm manager run loop failed");
        }
        self.cleanup_instances();
        self.cleanup_shared_host();
        self.manager_running.store(false, Ordering::SeqCst);
        info!("vm manager stopped");
        result
    }

    fn run_inner(&mut self) -> VmManagerResult<()> {
        let poll: PollContext<u8> = PollContext::new().unwrap();
        let mut routes = HashMap::new();
        let mut next_token_count = 1usize;

        poll.add(&self.command_evt.0, COMMAND_TOKEN)
            .map_err(|e| io::Error::from_raw_os_error(e.errno()))?;

        self.start_configured_vms(&poll, &mut routes, &mut next_token_count)?;

        loop {
            let events: PollEvents<u8> = poll.wait().unwrap();
            for event in events.iter_readable() {
                if event.token() == COMMAND_TOKEN {
                    self.handle_commands(&poll, &mut routes, &mut next_token_count)?;
                    continue;
                }

                let Some(route) = routes.get(&event.token()).copied() else {
                    continue;
                };

                let should_remove = match self.instances.get_mut(&route.vm_id) {
                    Some(instance) => instance.handle_event(
                        route.kind,
                        route.vm_id,
                        &poll,
                        &mut routes,
                        &mut next_token_count,
                    )?,
                    None => false,
                };

                if should_remove {
                    self.cleanup_instance(route.vm_id, &mut routes);
                    if self.instances.is_empty() {
                        self.cleanup_shared_host();
                    }
                }
            }
        }
    }

    fn handle_commands(
        &mut self,
        poll: &PollContext<u8>,
        routes: &mut HashMap<u8, EventRoute>,
        next_token_count: &mut usize,
    ) -> VmManagerResult<()> {
        let _ = self.command_evt.read();

        while let Ok(command) = self.command_rx.try_recv() {
            match command {
                VmCommand::Start {
                    name,
                    ssh_key,
                    response,
                } => {
                    info!(name = %name, "start vm command received");
                    let config = self.vm_config(name, ssh_key);
                    let _ = response.send(self.start_vm(config, poll, routes, next_token_count));
                }
                VmCommand::List { response } => {
                    debug!("list vms command received");
                    let _ = response.send(Ok(self.list_vms()));
                }
                VmCommand::Stop { id, response } => {
                    info!(id, "stop vm command received");
                    let _ = response.send(self.request_stop_vm(id));
                }
            }
        }

        Ok(())
    }

    fn request_stop_vm(&mut self, id: u8) -> VmManagerResult<()> {
        let instance = self.instances.get(&id).ok_or_else(|| {
            warn!(id, "stop requested for missing vm");
            io::Error::new(io::ErrorKind::NotFound, format!("vm {id} is not running"))
        })?;

        instance.request_stop()
    }

    fn start_configured_vms(
        &mut self,
        poll: &PollContext<u8>,
        routes: &mut HashMap<u8, EventRoute>,
        next_token_count: &mut usize,
    ) -> VmManagerResult<()> {
        let configs = std::mem::take(&mut self.configs);

        for config in configs.into_values() {
            self.start_vm(config, poll, routes, next_token_count)?;
        }

        Ok(())
    }

    fn next_avail_id(&mut self) -> u8 {
        let id = self.next_vm_id;
        self.next_vm_id += 1;
        id
    }

    fn vm_config(&mut self, name: String, ssh_key: String) -> VmConfig {
        VmConfig {
            id: self.next_avail_id(),
            name,
            ssh_key,
        }
    }

    fn start_vm(
        &mut self,
        config: VmConfig,
        poll: &PollContext<u8>,
        routes: &mut HashMap<u8, EventRoute>,
        next_token_count: &mut usize,
    ) -> VmManagerResult<VmInfo> {
        info!(vm_id = config.id(), name = %config.name(), "starting vm");
        if self.configs.contains_key(&config.id()) || self.instances.contains_key(&config.id()) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("duplicate vm id {}", config.id()),
            )
            .into());
        }

        let had_instances = !self.instances.is_empty();
        if !self.shared_host_provisioned {
            debug!("provisioning shared host");
            provision::provision_shared_host()?;
            self.shared_host_provisioned = true;
            debug!("shared host provisioned");
        }

        let vm_id = config.id();
        let instance = match VmInstance::start(config) {
            Ok(instance) => instance,
            Err(err) => {
                error!(vm_id, error = %err, "failed to start vm");
                cleanup_vm_resources(vm_id);
                if !had_instances {
                    self.cleanup_shared_host();
                }
                return Err(err);
            }
        };
        self.instances.insert(vm_id, instance);

        let instance = self.instances.get(&vm_id).unwrap();
        if let Err(err) = instance.register_events(vm_id, poll, routes, next_token_count) {
            error!(vm_id, error = %err, "failed to register vm events");
            self.cleanup_instance(vm_id, routes);
            if !had_instances {
                self.cleanup_shared_host();
            }
            return Err(err);
        }

        info!(vm_id, "vm started");
        Ok(instance.info())
    }

    fn list_vms(&self) -> Vec<VmInfo> {
        let mut vms = self
            .instances
            .values()
            .map(VmInstance::info)
            .collect::<Vec<_>>();
        vms.sort_by_key(|vm| vm.id);
        vms
    }

    fn cleanup_instance(&mut self, vm_id: u8, routes: &mut HashMap<u8, EventRoute>) {
        info!(vm_id, "cleaning up vm");
        routes.retain(|_, route| route.vm_id != vm_id);
        self.instances.remove(&vm_id);
        cleanup_vm_resources(vm_id);
        debug!(vm_id, "vm cleanup completed");
    }

    fn cleanup_instances(&mut self) {
        for vm_id in self.instances.keys().copied().collect::<Vec<_>>() {
            info!(vm_id, "cleaning up vm");
            cleanup_vm_resources(vm_id);
        }

        self.instances.clear();
    }

    fn cleanup_shared_host(&mut self) {
        if self.shared_host_provisioned {
            debug!("cleaning up shared host");
            if let Err(err) = provision::deprovision_shared_host() {
                warn!(error = %err, "failed to deprovision shared host");
            }
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
    name: String,
    ip_address: String,
    _vm: VmFd,
    vcpu_stopper: VcpuStopper,
    net: NetDeviceRuntime,
    blk: BlkDeviceRuntime,
    exit_evt: EventWrapper,
    tap_rx_registered: bool,
}

struct NetDeviceRuntime {
    device: Arc<Mutex<Net>>,
    activate_evt: EventFd,
    queue_events: Vec<EventFd>,
}

struct BlkDeviceRuntime {
    device: Arc<Mutex<Blk>>,
    queue_events: Vec<EventFd>,
}

impl VmInstance {
    fn start(config: VmConfig) -> VmManagerResult<Self> {
        let span = tracing::info_span!(
            "vm",
            vm_id = config.id(),
            name = %config.name(),
            guest_ip = %config.guest_ip()
        );
        let _entered = span.enter();

        install_stop_signal_handler()?;

        debug!("provisioning vm");
        Self::provision_vm(&config)?;

        debug!("booting vm");
        let BootArtifacts {
            vm,
            vcpu,
            guest_mem,
        } = Self::boot_vm(&config)?;
        let mut mmio_bus = MMIOBus::new();
        debug!("creating net device");
        let net = Self::create_net_device(&config, &vm, &guest_mem, &mut mmio_bus);
        debug!("creating block device");
        let blk = Self::create_blk_device(&config, &vm, &guest_mem, &mut mmio_bus);
        debug!("creating serial device");
        let stdio_serial = Self::create_serial(&vm);
        debug!("starting vcpu thread");
        let (vcpu_stopper, exit_evt) = Self::start_vcpu(config.id(), vcpu, stdio_serial, mmio_bus);

        Ok(Self {
            id: config.id(),
            name: config.name().to_string(),
            ip_address: config.guest_ip(),
            _vm: vm,
            vcpu_stopper,
            net,
            blk,
            exit_evt,
            tap_rx_registered: false,
        })
    }

    fn provision_vm(config: &VmConfig) -> VmManagerResult<()> {
        provision::provision_vm_host(config.id())?;
        provision::provision_guest(config.id(), config.ssh_key().trim_end())?;

        info!(
            vm_id = config.id(),
            rootfs = %config.rootfs_path().display(),
            tap = %config.tap_name(),
            guest_ip = %config.guest_ip(),
            gateway = "10.10.0.1",
            dns = "8.8.8.8",
            "provisioned vm"
        );

        Ok(())
    }

    fn boot_vm(_config: &VmConfig) -> VmManagerResult<BootArtifacts> {
        Ok(boot::setup(KERNEL_PATH))
    }

    fn create_net_device(
        config: &VmConfig,
        vm: &VmFd,
        guest_mem: &GuestMemoryMmap<()>,
        mmio_bus: &mut MMIOBus,
    ) -> NetDeviceRuntime {
        let virtio_net = net::Net::new(&config.tap_name(), Some(config.guest_mac()))
            .expect("failed to create virtio net");
        vm.register_irqfd(virtio_net.interrupt_event(), 5).unwrap();
        for (q_sel, evt) in virtio_net.queues_events().iter().enumerate() {
            let addr = IoEventAddress::Mmio(QUEUE_NOTIFY_OFFSET + MMIO_MEM_START);
            vm.register_ioevent(evt, &addr, q_sel as u32).unwrap();
        }

        let activate_evt = virtio_net.activate_event.try_clone().unwrap();
        let queue_events = virtio_net
            .queues_events()
            .iter()
            .map(|evt| evt.try_clone().unwrap())
            .collect::<Vec<_>>();
        let device = Arc::new(Mutex::new(virtio_net));
        mmio_bus
            .insert_virtio(MMIO_MEM_START, device.clone(), guest_mem.clone())
            .expect("insert virtio net mmio");

        NetDeviceRuntime {
            device,
            activate_evt,
            queue_events,
        }
    }

    fn create_blk_device(
        config: &VmConfig,
        vm: &VmFd,
        guest_mem: &GuestMemoryMmap<()>,
        mmio_bus: &mut MMIOBus,
    ) -> BlkDeviceRuntime {
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

        let queue_events = virtio_blk
            .queues_events()
            .iter()
            .map(|evt| evt.try_clone().unwrap())
            .collect::<Vec<_>>();
        let device = Arc::new(Mutex::new(virtio_blk));
        mmio_bus
            .insert_virtio(BLK_MMIO_MEM_START, device.clone(), guest_mem.clone())
            .expect("insert virtio blk mmio");

        BlkDeviceRuntime {
            device,
            queue_events,
        }
    }

    fn create_serial(vm: &VmFd) -> Arc<Mutex<HostSerial>> {
        let com_evt_1 = EventWrapper::new();
        vm.register_irqfd(&com_evt_1.0, 4).unwrap();

        Arc::new(Mutex::new(Serial::with_events(
            com_evt_1.try_clone().unwrap(),
            DummySerialEvent,
            std::io::sink(),
        )))
    }

    fn start_vcpu(
        vm_id: u8,
        vcpu: kvm_ioctls::VcpuFd,
        stdio_serial: Arc<Mutex<HostSerial>>,
        mmio_bus: MMIOBus,
    ) -> (VcpuStopper, EventWrapper) {
        let exit_evt = EventWrapper::new();
        let vcpu_stopper = VcpuStopper::new();
        spawn_vcpu_runtime(
            vm_id,
            vcpu,
            stdio_serial,
            mmio_bus,
            exit_evt.try_clone().unwrap(),
            vcpu_stopper.clone(),
        );

        (vcpu_stopper, exit_evt)
    }

    fn request_stop(&self) -> VmManagerResult<()> {
        self.vcpu_stopper.request_stop()?;
        Ok(())
    }

    fn info(&self) -> VmInfo {
        VmInfo {
            id: self.id,
            name: self.name.clone(),
            ip_address: self.ip_address.clone(),
            status: "running",
        }
    }

    fn register_events(
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
            &self.net.activate_evt,
            EventRoute {
                vm_id,
                kind: EventKind::NetActivate,
            },
            routes,
            next_token_count,
        )?;
        for (q_sel, evt) in self.net.queue_events.iter().enumerate() {
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
        for (q_sel, evt) in self.blk.queue_events.iter().enumerate() {
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

    fn handle_event(
        &mut self,
        kind: EventKind,
        vm_id: u8,
        poll: &PollContext<u8>,
        routes: &mut HashMap<u8, EventRoute>,
        next_token_count: &mut usize,
    ) -> VmManagerResult<bool> {
        match kind {
            EventKind::Exit => Ok(self.handle_exit()),
            EventKind::NetActivate => {
                self.handle_net_activate()?;
                self.register_tap_rx(vm_id, poll, routes, next_token_count)?;
                Ok(false)
            }
            EventKind::TapRx => Ok(self.handle_tap_rx()),
            EventKind::NetQueue(idx) => Ok(self.handle_net_queue(idx)),
            EventKind::BlkQueue(idx) => Ok(self.handle_blk_queue(idx)),
        }
    }

    fn handle_exit(&self) -> bool {
        info!(vm_id = self.id, "vcpu stopped");
        true
    }

    fn handle_net_activate(&mut self) -> io::Result<()> {
        self.net.activate_evt.read().map_err(|e| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("failed to read activate event: {e}"),
            )
        })?;
        Ok(())
    }

    fn handle_tap_rx(&mut self) -> bool {
        let mut net = self.net.device.lock().unwrap();
        if let Err(err) = net.process_rx() {
            warn!(vm_id = self.id, error = %err, "failed to process net rx");
        }
        false
    }

    fn handle_net_queue(&mut self, idx: usize) -> bool {
        if let Err(err) = self.net.queue_events[idx].read() {
            warn!(vm_id = self.id, queue = idx, error = %err, "failed to read net queue event");
        }
        let mut net = self.net.device.lock().unwrap();
        if let Err(err) = net.process_tx() {
            warn!(vm_id = self.id, queue = idx, error = %err, "failed to process net tx");
        }
        false
    }

    fn handle_blk_queue(&mut self, idx: usize) -> bool {
        if let Err(err) = self.blk.queue_events[idx].read() {
            warn!(vm_id = self.id, queue = idx, error = %err, "failed to read block queue event");
        }
        let mut blk = self.blk.device.lock().unwrap();
        if let Err(err) = blk.process_queue() {
            warn!(vm_id = self.id, queue = idx, error = %err, "failed to process block queue");
        }
        false
    }

    fn register_tap_rx(
        &mut self,
        vm_id: u8,
        poll: &PollContext<u8>,
        routes: &mut HashMap<u8, EventRoute>,
        next_token_count: &mut usize,
    ) -> VmManagerResult<()> {
        if self.tap_rx_registered {
            return Ok(());
        }

        let token = next_token(next_token_count)?;
        self.net
            .device
            .lock()
            .unwrap()
            .register_tap_readable(poll, token)?;
        routes.insert(
            token,
            EventRoute {
                vm_id,
                kind: EventKind::TapRx,
            },
        );
        self.tap_rx_registered = true;
        debug!(vm_id, token, "registered tap rx event");

        Ok(())
    }
}

fn cleanup_vm_resources(vm_id: u8) {
    if let Err(err) = provision::deprovision_guest(vm_id) {
        warn!(vm_id, error = %err, "failed to deprovision guest resources");
    }
    if let Err(err) = provision::deprovision_vm_host(vm_id) {
        warn!(vm_id, error = %err, "failed to deprovision vm host resources");
    }
}

fn spawn_vcpu_runtime(
    vm_id: u8,
    mut vcpu: kvm_ioctls::VcpuFd,
    stdio_serial: Arc<Mutex<HostSerial>>,
    mut mmio_bus: MMIOBus,
    vcpu_exit_evt: EventWrapper,
    vcpu_stopper: VcpuStopper,
) {
    std::thread::spawn(move || {
        unblock_stop_signal();
        vcpu_stopper.set_thread();
        vcpu_stopper.set_kvm_run(vcpu.get_kvm_run() as *mut _);

        loop {
            if vcpu_stopper.stop_requested() {
                break;
            }

            match vcpu.run() {
                Ok(run) => match run {
                    VcpuExit::IoIn(addr, data) => {
                        if addr >= COM1 && addr - COM1 < 8 {
                            data[0] = lock_host_serial(&stdio_serial).read((addr - COM1) as u8)
                        }
                    }
                    VcpuExit::IoOut(addr, data) => {
                        if addr >= COM1 && addr - COM1 < 8 {
                            let _ =
                                lock_host_serial(&stdio_serial).write((addr - COM1) as u8, data[0]);
                        }
                    }
                    VcpuExit::MmioRead(addr, data) => {
                        mmio_bus.read(addr, data);
                    }
                    VcpuExit::MmioWrite(addr, data) => {
                        mmio_bus.write(addr, data);
                    }
                    VcpuExit::Hlt => {
                        info!(vm_id, "vcpu halted");
                        break;
                    }
                    VcpuExit::Shutdown => {
                        info!(vm_id, "vcpu shutdown");
                        break;
                    }
                    r => {
                        debug!(vm_id, exit = ?r, "vcpu exit");
                    }
                },
                Err(e) => {
                    if vcpu_stopper.stop_requested()
                        && e == vmm_sys_util::errno::Error::new(libc::EINTR)
                    {
                        debug!(vm_id, "vcpu interrupted for stop request");
                        break;
                    }

                    error!(vm_id, error = ?e, "kvm run failed");
                    break;
                }
            }
        }
        vcpu_stopper.clear_thread();
        vcpu_exit_evt.trigger().unwrap();
    });
}

#[derive(Clone)]
struct VcpuStopper {
    stop_requested: Arc<AtomicBool>,
    thread: Arc<Mutex<Option<libc::pthread_t>>>,
    kvm_run: Arc<AtomicPtr<kvm_bindings::kvm_run>>,
}

impl VcpuStopper {
    fn new() -> Self {
        Self {
            stop_requested: Arc::new(AtomicBool::new(false)),
            thread: Arc::new(Mutex::new(None)),
            kvm_run: Arc::new(AtomicPtr::new(ptr::null_mut())),
        }
    }

    fn set_thread(&self) {
        *self.thread.lock().unwrap() = Some(unsafe { libc::pthread_self() });
    }

    fn clear_thread(&self) {
        let mut thread = self.thread.lock().unwrap();
        self.kvm_run.store(ptr::null_mut(), Ordering::SeqCst);
        *thread = None;
    }

    fn set_kvm_run(&self, kvm_run: *mut kvm_bindings::kvm_run) {
        self.kvm_run.store(kvm_run, Ordering::SeqCst);
    }

    fn request_stop(&self) -> io::Result<()> {
        let was_already_set = self.stop_requested.swap(true, Ordering::SeqCst);
        if was_already_set {
            return Ok(());
        }

        let kvm_run = self.kvm_run.load(Ordering::SeqCst);
        if !kvm_run.is_null() {
            unsafe {
                ptr::write_volatile(ptr::addr_of_mut!((*kvm_run).immediate_exit), 1);
            }
        }

        let thread = self.thread.lock().unwrap();
        let Some(thread) = *thread else {
            return Ok(());
        };

        let ret = unsafe { libc::pthread_kill(thread, VM_STOP_SIGNAL) };
        if ret != 0 && ret != libc::ESRCH {
            return Err(io::Error::from_raw_os_error(ret));
        }
        Ok(())
    }

    fn stop_requested(&self) -> bool {
        self.stop_requested.load(Ordering::SeqCst)
    }
}

extern "C" fn handle_stop_signal(_signal: libc::c_int) {}

fn install_stop_signal_handler() -> io::Result<()> {
    match *STOP_SIGNAL_HANDLER_RESULT.get_or_init(|| unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handle_stop_signal as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = 0;
        let ret = libc::sigaction(VM_STOP_SIGNAL, &action, std::ptr::null_mut());
        if ret == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EINVAL))
        }
    }) {
        Ok(()) => Ok(()),
        Err(errno) => Err(io::Error::from_raw_os_error(errno)),
    }
}

fn unblock_stop_signal() {
    unsafe {
        let mut signals: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut signals);
        libc::sigaddset(&mut signals, VM_STOP_SIGNAL);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &signals, std::ptr::null_mut());
    }
}

fn lock_host_serial(serial: &Arc<Mutex<HostSerial>>) -> std::sync::MutexGuard<'_, HostSerial> {
    match serial.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
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

    fn test_config(id: u8) -> VmConfig {
        VmConfig {
            id,
            name: format!("vm-{id}"),
            ssh_key: "ssh-ed25519 test".to_string(),
        }
    }

    #[test]
    fn vm_config_derives_identity_from_id() {
        let config = test_config(7);

        assert_eq!(config.tap_name(), "tap7");
        assert_eq!(config.guest_ip(), "10.10.0.9");
        assert_eq!(config.guest_mac(), [0x02, 0x12, 0x34, 0x56, 0x78, 7]);
        assert_eq!(config.rootfs_path(), PathBuf::from("/tmp/vm-7/rootfs.ext4"));
    }

    #[test]
    fn add_vm_rejects_duplicate_ids() {
        let mut manager = VmManager::new();
        let config = test_config(1);

        manager.add_vm(config.clone()).unwrap();

        let err = manager.add_vm(config).unwrap_err();
        assert_eq!(
            err.downcast_ref::<io::Error>().unwrap().kind(),
            ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn request_stop_vm_rejects_missing_instance() {
        let mut manager = VmManager::new();

        let err = manager.request_stop_vm(1).unwrap_err();

        assert_eq!(
            err.downcast_ref::<io::Error>().unwrap().kind(),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn handle_commands_replies_to_stop_for_missing_instance() {
        let mut manager = VmManager::new();
        let poll: PollContext<u8> = PollContext::new().unwrap();
        let mut routes = HashMap::new();
        let mut next_token_count = 0;
        let (response_tx, response_rx) = mpsc::channel();

        manager
            .command_tx
            .send(VmCommand::Stop {
                id: 1,
                response: response_tx,
            })
            .unwrap();

        manager
            .handle_commands(&poll, &mut routes, &mut next_token_count)
            .unwrap();

        let err = response_rx.recv().unwrap().unwrap_err();
        assert_eq!(
            err.downcast_ref::<io::Error>().unwrap().kind(),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn handle_commands_replies_to_list_with_running_vms() {
        let mut manager = VmManager::new();
        let poll: PollContext<u8> = PollContext::new().unwrap();
        let mut routes = HashMap::new();
        let mut next_token_count = 0;
        let (response_tx, response_rx) = mpsc::channel();

        manager
            .command_tx
            .send(VmCommand::List {
                response: response_tx,
            })
            .unwrap();

        manager
            .handle_commands(&poll, &mut routes, &mut next_token_count)
            .unwrap();

        assert_eq!(response_rx.recv().unwrap().unwrap(), Vec::new());
    }

    #[test]
    fn handle_request_stop_vm_rejects_when_manager_is_not_running() {
        let manager = VmManager::new();
        let handle = manager.handle().unwrap();

        let err = handle.request_stop_vm(1).unwrap_err();

        assert_eq!(
            err.downcast_ref::<io::Error>().unwrap().kind(),
            ErrorKind::NotConnected
        );
    }

    #[test]
    fn handle_start_vm_rejects_when_manager_is_not_running() {
        let manager = VmManager::new();
        let handle = manager.handle().unwrap();

        let err = handle
            .start_vm("test-vm".to_string(), "ssh-ed25519 test".to_string())
            .unwrap_err();

        assert_eq!(
            err.downcast_ref::<io::Error>().unwrap().kind(),
            ErrorKind::NotConnected
        );
    }

    #[test]
    fn next_avail_id_returns_current_id_then_advances() {
        let mut manager = VmManager::new();

        assert_eq!(manager.next_avail_id(), 0);
        assert_eq!(manager.next_avail_id(), 1);
    }

    #[test]
    fn handle_list_vms_rejects_when_manager_is_not_running() {
        let manager = VmManager::new();
        let handle = manager.handle().unwrap();

        let err = handle.list_vms().unwrap_err();

        assert_eq!(
            err.downcast_ref::<io::Error>().unwrap().kind(),
            ErrorKind::NotConnected
        );
    }

    #[test]
    fn vcpu_stopper_records_stop_before_thread_registration() {
        let stopper = VcpuStopper::new();

        stopper.request_stop().unwrap();
        stopper.request_stop().unwrap();

        assert!(stopper.stop_requested());
    }

    // Token count go up to 255, then no more token.
    #[test]
    fn next_token_uses_full_u8_range_then_exhausts() {
        let mut next_token_count = 254;

        assert_eq!(next_token(&mut next_token_count).unwrap(), 254);
        assert_eq!(next_token(&mut next_token_count).unwrap(), 255);
        assert_eq!(next_token_count, 256);

        let err = next_token(&mut next_token_count).unwrap_err();
        assert_eq!(
            err.downcast_ref::<io::Error>().unwrap().kind(),
            ErrorKind::Other
        );
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
    fn cleanup_instance_prunes_only_target_vm_routes() {
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

        manager.cleanup_instance(3, &mut routes);

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
    fn missing_instance_route_is_ignored() {
        let mut manager = VmManager::new();
        let poll: PollContext<u8> = PollContext::new().unwrap();
        let mut routes = HashMap::new();
        let mut next_token_count = 0;

        let should_remove = match manager.instances.get_mut(&11) {
            Some(instance) => instance
                .handle_event(
                    EventKind::Exit,
                    11,
                    &poll,
                    &mut routes,
                    &mut next_token_count,
                )
                .unwrap(),
            None => false,
        };

        assert!(!should_remove);
    }
}
