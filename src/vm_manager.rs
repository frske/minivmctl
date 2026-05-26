use std::{
    collections::HashMap,
    fs::OpenOptions,
    io,
    os::fd::AsRawFd,
    path::PathBuf,
    ptr,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use event_manager::{
    EventManager, EventOps, Events, MutEventSubscriber, RemoteEndpoint, SubscriberId, SubscriberOps,
};
use kvm_ioctls::{IoEventAddress, VcpuExit, VmFd};
use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet, SigmaskHow, Signal};
use tracing::{debug, error, info, warn};
use virtio_bindings::virtio_mmio::VIRTIO_MMIO_QUEUE_NOTIFY;
use vm_memory::GuestMemoryMmap;
use vm_superio::{Serial, Trigger};
use vmm_sys_util::epoll::EventSet;
use vmm_sys_util::eventfd::EventFd;

use crate::{
    blk::Blk,
    boot::{self, BootArtifacts},
    bus::{MMIOBus, VIRTIO_MMIO_SIZE},
    event::EventWrapper,
    net::{self, Net},
    provision,
    serial::DummySerialEvent,
};

type HostSerial = Serial<EventWrapper, DummySerialEvent, std::io::Sink>;
pub type VmManagerResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const MMIO_MEM_START: u64 = 0xd0000000;
const BLK_MMIO_MEM_START: u64 = MMIO_MEM_START + VIRTIO_MMIO_SIZE;
const QUEUE_NOTIFY_OFFSET: u64 = VIRTIO_MMIO_QUEUE_NOTIFY as u64;
const COM1: u16 = 0x3f8;
const KERNEL_PATH: &str = "/home/frsk/prj/kernel_images/linux-6.6.99/vmlinux";
const VM_STOP_SIGNAL: Signal = Signal::SIGUSR1;
static STOP_SIGNAL_HANDLER_RESULT: OnceLock<Result<(), i32>> = OnceLock::new();

pub struct VmManagerHandle {
    endpoint: RemoteEndpoint<VmInstance>,
    registry: Arc<Mutex<VmRegistry>>,
    manager_running: Arc<AtomicBool>,
}

impl Clone for VmManagerHandle {
    fn clone(&self) -> Self {
        Self {
            endpoint: self.endpoint.clone(),
            registry: Arc::clone(&self.registry),
            manager_running: Arc::clone(&self.manager_running),
        }
    }
}

impl VmManagerHandle {
    pub fn start_vm(
        &self,
        name: String,
        ssh_key: String,
        memory_mib: u32,
    ) -> VmManagerResult<VmInfo> {
        let registry = Arc::clone(&self.registry);
        let endpoint = self.endpoint.clone();
        self.call(Duration::from_secs(120), move |ops| {
            let config = allocate_vm_config(&registry, name, ssh_key, memory_mib)?;
            let vm_id = config.id();
            let had_instances = has_running_vms(&registry);

            if !is_shared_host_provisioned(&registry) {
                debug!("provisioning shared host");
                provision::provision_shared_host()?;
                set_shared_host_provisioned(&registry, true);
                debug!("shared host provisioned");
            }

            let subscriber_id = Arc::new(Mutex::new(None));
            let init_error = Arc::new(Mutex::new(None));
            let instance = match VmInstance::start(
                config,
                Arc::clone(&registry),
                endpoint,
                Arc::clone(&subscriber_id),
                Arc::clone(&init_error),
            ) {
                Ok(instance) => instance,
                Err(err) => {
                    error!(vm_id, error = %err, "failed to start vm");
                    cleanup_vm_resources(vm_id);
                    if !had_instances {
                        cleanup_shared_host(&registry);
                    }
                    return Err(err);
                }
            };

            let info = instance.info();
            let stopper = instance.vcpu_stopper.clone();
            let id = ops.add_subscriber(instance);
            *subscriber_id.lock().unwrap() = Some(id);
            if let Some(err) = init_error.lock().unwrap().take() {
                let _ = ops.remove_subscriber(id);
                if !had_instances {
                    cleanup_shared_host(&registry);
                }
                return Err(err);
            }
            insert_vm_entry(&registry, id, info.clone(), stopper);
            info!(vm_id, "vm started");
            Ok(info)
        })
    }

    pub fn list_vms(&self) -> VmManagerResult<Vec<VmInfo>> {
        self.ensure_running()?;
        Ok(list_registry_vms(&self.registry))
    }

    /// Requests a hard stop and returns when the manager accepts the request.
    /// VM cleanup completes later through the existing vCPU exit event path.
    pub fn request_stop_vm(&self, id: u8) -> VmManagerResult<()> {
        self.ensure_running()?;
        let registry = Arc::clone(&self.registry);
        self.call(Duration::from_secs(5), move |_ops| {
            let stopper = vm_stopper(&registry, id)?;
            stopper.request_stop()?;
            Ok(())
        })
    }

    pub fn shutdown(&self) -> VmManagerResult<()> {
        self.ensure_running()?;
        let registry = Arc::clone(&self.registry);
        self.call(Duration::from_secs(5), move |_ops| {
            mark_registry_shutting_down(&registry);
            request_stop_all_registry_vms(&registry)
        })
    }

    pub fn wait_until_running(&self, timeout: Duration) -> VmManagerResult<()> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if self.manager_running.load(Ordering::SeqCst) {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }

        Err(io::Error::new(io::ErrorKind::TimedOut, "vm manager did not start").into())
    }

    fn ensure_running(&self) -> VmManagerResult<()> {
        if !self.manager_running.load(Ordering::SeqCst) {
            return Err(
                io::Error::new(io::ErrorKind::NotConnected, "vm manager is not running").into(),
            );
        }

        Ok(())
    }

    fn call<T>(
        &self,
        timeout: Duration,
        f: impl FnOnce(&mut dyn SubscriberOps<Subscriber = VmInstance>) -> VmManagerResult<T>
        + Send
        + 'static,
    ) -> VmManagerResult<T>
    where
        T: Send + 'static,
    {
        self.ensure_running()?;

        let (response_tx, response_rx) = mpsc::channel();
        self.endpoint.fire(move |ops| {
            let _ = response_tx.send(f(ops));
        })?;

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
    pub memory_mib: u32,
    pub status: &'static str,
}

#[derive(Default)]
struct VmRegistry {
    next_vm_id: u8,
    entries: HashMap<u8, VmEntry>,
    shared_host_provisioned: bool,
    shutting_down: bool,
}

struct VmEntry {
    info: VmInfo,
    stopper: VcpuStopper,
}

const EXIT_EVENT: u32 = 0;
const NET_ACTIVATE_EVENT: u32 = 1;
const TAP_RX_EVENT: u32 = 2;
const NET_QUEUE_EVENT_BASE: u32 = 100;
const BLK_QUEUE_EVENT_BASE: u32 = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmConfig {
    id: u8,
    name: String,
    ssh_key: String,
    memory_mib: u32,
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

    pub fn memory_mib(&self) -> u32 {
        self.memory_mib
    }

    pub fn memory_size(&self) -> usize {
        self.memory_mib as usize * 1024 * 1024
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

fn allocate_vm_config(
    registry: &Arc<Mutex<VmRegistry>>,
    name: String,
    ssh_key: String,
    memory_mib: u32,
) -> VmManagerResult<VmConfig> {
    let mut registry = registry.lock().unwrap();
    if registry.shutting_down {
        return Err(
            io::Error::new(io::ErrorKind::NotConnected, "vm manager is shutting down").into(),
        );
    }
    if registry.next_vm_id > 253 {
        return Err(io::Error::other("exhausted vm ids").into());
    }

    let id = registry.next_vm_id;
    registry.next_vm_id = registry
        .next_vm_id
        .checked_add(1)
        .ok_or_else(|| io::Error::other("exhausted vm ids"))?;

    Ok(VmConfig {
        id,
        name,
        ssh_key,
        memory_mib,
    })
}

fn insert_vm_entry(
    registry: &Arc<Mutex<VmRegistry>>,
    _subscriber_id: SubscriberId,
    info: VmInfo,
    stopper: VcpuStopper,
) {
    registry
        .lock()
        .unwrap()
        .entries
        .insert(info.id, VmEntry { info, stopper });
}

fn remove_vm_entry(registry: &Arc<Mutex<VmRegistry>>, id: u8) -> bool {
    let mut registry = registry.lock().unwrap();
    registry.entries.remove(&id).is_some()
}

fn list_registry_vms(registry: &Arc<Mutex<VmRegistry>>) -> Vec<VmInfo> {
    let mut vms = registry
        .lock()
        .unwrap()
        .entries
        .values()
        .map(|entry| entry.info.clone())
        .collect::<Vec<_>>();
    vms.sort_by_key(|vm| vm.id);
    vms
}

fn vm_stopper(registry: &Arc<Mutex<VmRegistry>>, id: u8) -> VmManagerResult<VcpuStopper> {
    registry
        .lock()
        .unwrap()
        .entries
        .get(&id)
        .map(|entry| entry.stopper.clone())
        .ok_or_else(|| {
            warn!(id, "stop requested for missing vm");
            io::Error::new(io::ErrorKind::NotFound, format!("vm {id} is not running")).into()
        })
}

fn has_running_vms(registry: &Arc<Mutex<VmRegistry>>) -> bool {
    !registry.lock().unwrap().entries.is_empty()
}

fn is_shared_host_provisioned(registry: &Arc<Mutex<VmRegistry>>) -> bool {
    registry.lock().unwrap().shared_host_provisioned
}

fn set_shared_host_provisioned(registry: &Arc<Mutex<VmRegistry>>, provisioned: bool) {
    registry.lock().unwrap().shared_host_provisioned = provisioned;
}

fn cleanup_shared_host(registry: &Arc<Mutex<VmRegistry>>) {
    let should_cleanup = {
        let mut registry = registry.lock().unwrap();
        if !registry.shared_host_provisioned {
            false
        } else {
            registry.shared_host_provisioned = false;
            true
        }
    };

    if should_cleanup {
        debug!("cleaning up shared host");
        if let Err(err) = provision::deprovision_shared_host() {
            warn!(error = %err, "failed to deprovision shared host");
        }
    }
}

fn mark_registry_shutting_down(registry: &Arc<Mutex<VmRegistry>>) {
    registry.lock().unwrap().shutting_down = true;
}

fn registry_should_exit(registry: &Arc<Mutex<VmRegistry>>) -> bool {
    let registry = registry.lock().unwrap();
    registry.shutting_down && registry.entries.is_empty()
}

fn request_stop_all_registry_vms(registry: &Arc<Mutex<VmRegistry>>) -> VmManagerResult<()> {
    let entries = registry
        .lock()
        .unwrap()
        .entries
        .iter()
        .map(|(&id, entry)| (id, entry.stopper.clone()))
        .collect::<Vec<_>>();
    let mut first_error = None;

    for (id, stopper) in entries {
        if let Err(err) = stopper.request_stop() {
            warn!(vm_id = id, error = %err, "failed to request vm stop");
            if first_error.is_none() {
                first_error = Some(err);
            }
        }
    }

    first_error.map_or(Ok(()), |err| Err(err.into()))
}

pub struct VmManager {
    event_manager: Option<EventManager<VmInstance>>,
    registry: Arc<Mutex<VmRegistry>>,
    endpoint: RemoteEndpoint<VmInstance>,
    manager_running: Arc<AtomicBool>,
}

impl VmManager {
    pub fn new() -> Self {
        let event_manager = EventManager::new().expect("failed to create vm event manager");
        let endpoint = event_manager.remote_endpoint();
        Self {
            event_manager: Some(event_manager),
            registry: Arc::new(Mutex::new(VmRegistry::default())),
            endpoint,
            manager_running: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn handle(&self) -> VmManagerResult<VmManagerHandle> {
        Ok(VmManagerHandle {
            endpoint: self.endpoint.clone(),
            registry: Arc::clone(&self.registry),
            manager_running: Arc::clone(&self.manager_running),
        })
    }

    pub fn run(&mut self) -> VmManagerResult<()> {
        info!("vm manager starting");
        self.manager_running.store(true, Ordering::SeqCst);
        let result = self.run_inner();
        if let Err(err) = &result {
            error!(error = %err, "vm manager run loop failed");
        }
        self.cleanup_instances();
        self.event_manager.take();
        self.cleanup_shared_host();
        self.manager_running.store(false, Ordering::SeqCst);
        info!("vm manager stopped");
        result
    }

    fn run_inner(&mut self) -> VmManagerResult<()> {
        let event_manager = self
            .event_manager
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "vm manager already ran"))?;

        while !registry_should_exit(&self.registry) {
            event_manager.run()?;
        }

        Ok(())
    }

    fn cleanup_instances(&mut self) {
        let _ = request_stop_all_registry_vms(&self.registry);
    }

    fn cleanup_shared_host(&mut self) {
        cleanup_shared_host(&self.registry);
    }
}

impl Default for VmManager {
    fn default() -> Self {
        Self::new()
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
    memory_mib: u32,
    registry: Arc<Mutex<VmRegistry>>,
    endpoint: RemoteEndpoint<VmInstance>,
    subscriber_id: Arc<Mutex<Option<SubscriberId>>>,
    init_error: Arc<Mutex<Option<Box<dyn std::error::Error + Send + Sync>>>>,
    _vm: VmFd,
    vcpu_stopper: VcpuStopper,
    vcpu_thread: Option<JoinHandle<()>>,
    net: NetDeviceRuntime,
    blk: BlkDeviceRuntime,
    exit_evt: EventWrapper,
    tap_rx_registered: bool,
    cleaned_up: bool,
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
    fn start(
        config: VmConfig,
        registry: Arc<Mutex<VmRegistry>>,
        endpoint: RemoteEndpoint<VmInstance>,
        subscriber_id: Arc<Mutex<Option<SubscriberId>>>,
        init_error: Arc<Mutex<Option<Box<dyn std::error::Error + Send + Sync>>>>,
    ) -> VmManagerResult<Self> {
        let span = tracing::info_span!(
            "vm",
            vm_id = config.id(),
            name = %config.name(),
            memory_mib = config.memory_mib(),
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
        let (vcpu_stopper, vcpu_thread, exit_evt) =
            Self::start_vcpu(config.id(), vcpu, stdio_serial, mmio_bus);

        Ok(Self {
            id: config.id(),
            name: config.name().to_string(),
            ip_address: config.guest_ip(),
            memory_mib: config.memory_mib(),
            registry,
            endpoint,
            subscriber_id,
            init_error,
            _vm: vm,
            vcpu_stopper,
            vcpu_thread: Some(vcpu_thread),
            net,
            blk,
            exit_evt,
            tap_rx_registered: false,
            cleaned_up: false,
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

    fn boot_vm(config: &VmConfig) -> VmManagerResult<BootArtifacts> {
        Ok(boot::setup(KERNEL_PATH, config.memory_size()))
    }

    fn create_net_device(
        config: &VmConfig,
        vm: &VmFd,
        guest_mem: &GuestMemoryMmap<()>,
        mmio_bus: &mut MMIOBus,
    ) -> NetDeviceRuntime {
        let virtio_net = net::Net::new(
            &config.tap_name(),
            Some(config.guest_mac()),
            guest_mem.clone(),
        )
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
            .insert_virtio(MMIO_MEM_START, device.clone())
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
            guest_mem.clone(),
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
            .insert_virtio(BLK_MMIO_MEM_START, device.clone())
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
    ) -> (VcpuStopper, JoinHandle<()>, EventWrapper) {
        let exit_evt = EventWrapper::new();
        let vcpu_stopper = VcpuStopper::new();
        let vcpu_thread = spawn_vcpu_runtime(
            vm_id,
            vcpu,
            stdio_serial,
            mmio_bus,
            exit_evt.try_clone().unwrap(),
            vcpu_stopper.clone(),
        );

        (vcpu_stopper, vcpu_thread, exit_evt)
    }

    fn info(&self) -> VmInfo {
        VmInfo {
            id: self.id,
            name: self.name.clone(),
            ip_address: self.ip_address.clone(),
            memory_mib: self.memory_mib,
            status: "running",
        }
    }

    fn join_vcpu_thread(&mut self) {
        if let Some(thread) = self.vcpu_thread.take()
            && thread.join().is_err()
        {
            warn!(vm_id = self.id, "vcpu thread panicked during cleanup");
        }
    }

    fn cleanup(&mut self, ops: &mut EventOps) {
        if self.cleaned_up {
            return;
        }

        info!(vm_id = self.id, "cleaning up vm");
        self.unregister_events(ops);
        self.join_vcpu_thread();
        cleanup_vm_resources(self.id);
        self.cleaned_up = true;
        debug!(vm_id = self.id, "vm cleanup completed");
    }

    fn schedule_remove_subscriber(&self) {
        let Some(subscriber_id) = *self.subscriber_id.lock().unwrap() else {
            return;
        };

        if let Err(err) = self.endpoint.fire(move |ops| {
            let _ = ops.remove_subscriber(subscriber_id);
        }) {
            warn!(vm_id = self.id, error = %err, "failed to schedule vm subscriber removal");
        }
    }

    fn unregister_events(&self, ops: &mut EventOps) {
        let mut events = vec![
            (self.exit_evt.0.as_raw_fd(), EXIT_EVENT),
            (self.net.activate_evt.as_raw_fd(), NET_ACTIVATE_EVENT),
        ];

        if self.tap_rx_registered {
            events.push((self.net.device.lock().unwrap().tap_raw_fd(), TAP_RX_EVENT));
        }

        for (q_sel, evt) in self.net.queue_events.iter().enumerate() {
            events.push((evt.as_raw_fd(), NET_QUEUE_EVENT_BASE + q_sel as u32));
        }
        for (q_sel, evt) in self.blk.queue_events.iter().enumerate() {
            events.push((evt.as_raw_fd(), BLK_QUEUE_EVENT_BASE + q_sel as u32));
        }

        for (fd, event) in events {
            if let Err(err) = ops.remove(Events::with_data_raw(fd, event, EventSet::IN)) {
                warn!(vm_id = self.id, event, error = %err, "failed to unregister vm event");
            }
        }
    }

    fn register_events(&self, ops: &mut EventOps) -> VmManagerResult<()> {
        ops.add(Events::with_data_raw(
            self.exit_evt.0.as_raw_fd(),
            EXIT_EVENT,
            EventSet::IN,
        ))?;
        ops.add(Events::with_data_raw(
            self.net.activate_evt.as_raw_fd(),
            NET_ACTIVATE_EVENT,
            EventSet::IN,
        ))?;

        for (q_sel, evt) in self.net.queue_events.iter().enumerate() {
            ops.add(Events::with_data_raw(
                evt.as_raw_fd(),
                NET_QUEUE_EVENT_BASE + q_sel as u32,
                EventSet::IN,
            ))?;
        }
        for (q_sel, evt) in self.blk.queue_events.iter().enumerate() {
            ops.add(Events::with_data_raw(
                evt.as_raw_fd(),
                BLK_QUEUE_EVENT_BASE + q_sel as u32,
                EventSet::IN,
            ))?;
        }

        Ok(())
    }

    fn handle_event(&mut self, event: u32, ops: &mut EventOps) -> VmManagerResult<bool> {
        if event >= BLK_QUEUE_EVENT_BASE {
            return Ok(self.handle_blk_queue((event - BLK_QUEUE_EVENT_BASE) as usize));
        }
        if event >= NET_QUEUE_EVENT_BASE {
            return Ok(self.handle_net_queue((event - NET_QUEUE_EVENT_BASE) as usize));
        }

        match event {
            EXIT_EVENT => Ok(self.handle_exit()),
            NET_ACTIVATE_EVENT => {
                self.handle_net_activate()?;
                self.register_tap_rx(ops)?;
                Ok(false)
            }
            TAP_RX_EVENT => Ok(self.handle_tap_rx()),
            _ => {
                warn!(vm_id = self.id, event, "ignoring unknown vm event");
                Ok(false)
            }
        }
    }

    fn register_tap_rx(&mut self, ops: &mut EventOps) -> VmManagerResult<()> {
        if self.tap_rx_registered {
            return Ok(());
        }

        let fd = self.net.device.lock().unwrap().tap_raw_fd();
        ops.add(Events::with_data_raw(fd, TAP_RX_EVENT, EventSet::IN))?;
        self.tap_rx_registered = true;
        debug!(vm_id = self.id, "registered tap rx event");
        Ok(())
    }

    fn handle_exit(&self) -> bool {
        info!(vm_id = self.id, "vcpu stopped");
        true
    }

    fn handle_net_activate(&mut self) -> io::Result<()> {
        self.net
            .activate_evt
            .read()
            .map_err(|e| io::Error::other(format!("failed to read activate event: {e}")))?;
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
}

impl Drop for VmInstance {
    fn drop(&mut self) {
        if self.cleaned_up {
            return;
        }

        if let Err(err) = self.vcpu_stopper.request_stop() {
            warn!(vm_id = self.id, error = %err, "failed to request vm stop during drop");
        }
        self.join_vcpu_thread();
        cleanup_vm_resources(self.id);
        let _ = remove_vm_entry(&self.registry, self.id);
    }
}

impl MutEventSubscriber for VmInstance {
    fn init(&mut self, ops: &mut EventOps) {
        if let Err(err) = self.register_events(ops) {
            error!(vm_id = self.id, error = %err, "failed to register vm events");
            *self.init_error.lock().unwrap() = Some(err);
        }
    }

    fn process(&mut self, events: Events, ops: &mut EventOps) {
        if !events.event_set().contains(EventSet::IN) {
            warn!(
                vm_id = self.id,
                event = events.data(),
                fd = events.fd(),
                events = ?events.event_set(),
                "ignoring non-readable vm event"
            );
            return;
        }

        let should_remove = match self.handle_event(events.data(), ops) {
            Ok(should_remove) => should_remove,
            Err(err) => {
                error!(vm_id = self.id, error = %err, "failed to handle vm event");
                return;
            }
        };

        if should_remove {
            self.cleanup(ops);
            if remove_vm_entry(&self.registry, self.id) && !has_running_vms(&self.registry) {
                cleanup_shared_host(&self.registry);
            }
            self.schedule_remove_subscriber();
        }
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
) -> JoinHandle<()> {
    thread::spawn(move || {
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
        if let Err(err) = vcpu_exit_evt.trigger() {
            error!(vm_id, error = %err, "failed to signal vcpu exit");
        }
    })
}

#[derive(Clone)]
struct VcpuStopper {
    stop_requested: Arc<AtomicBool>,
    state: Arc<Mutex<VcpuStopState>>,
}

#[derive(Default)]
struct VcpuStopState {
    thread: Option<libc::pthread_t>,
    kvm_run_addr: usize,
}

impl VcpuStopper {
    fn new() -> Self {
        Self {
            stop_requested: Arc::new(AtomicBool::new(false)),
            state: Arc::new(Mutex::new(VcpuStopState::default())),
        }
    }

    fn set_thread(&self) {
        // SAFETY: called from the vCPU thread itself; the pthread id is stored only for best-effort
        // interrupt delivery and cleared before thread exit.
        self.state.lock().unwrap().thread = Some(unsafe { libc::pthread_self() });
    }

    fn clear_thread(&self) {
        let mut state = self.state.lock().unwrap();
        state.kvm_run_addr = 0;
        state.thread = None;
    }

    fn set_kvm_run(&self, kvm_run: *mut kvm_bindings::kvm_run) {
        self.state.lock().unwrap().kvm_run_addr = kvm_run as usize;
    }

    fn request_stop(&self) -> io::Result<()> {
        let was_already_set = self.stop_requested.swap(true, Ordering::SeqCst);
        if was_already_set {
            return Ok(());
        }

        let state = self.state.lock().unwrap();
        if state.kvm_run_addr != 0 {
            let kvm_run = state.kvm_run_addr as *mut kvm_bindings::kvm_run;
            // SAFETY: pointer comes from live `VcpuFd::get_kvm_run()` in the vCPU thread. The same
            // mutex guards pointer publication and clearing, so it cannot be cleared while this
            // write is in progress. Volatile write is required by KVM immediate_exit.
            unsafe {
                ptr::write_volatile(ptr::addr_of_mut!((*kvm_run).immediate_exit), 1);
            }
        }

        let Some(thread) = state.thread else {
            return Ok(());
        };

        // SAFETY: `thread` came from `pthread_self` in the vCPU thread and is cleared when that
        // thread exits. ESRCH is accepted because the thread may exit concurrently.
        let ret = unsafe { libc::pthread_kill(thread, VM_STOP_SIGNAL as libc::c_int) };
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
    // SAFETY: handler is async-signal-safe because it performs no work; signal is used only to
    // interrupt KVM_RUN after `immediate_exit` is set.
    match *STOP_SIGNAL_HANDLER_RESULT.get_or_init(|| unsafe {
        let action = SigAction::new(
            SigHandler::Handler(handle_stop_signal),
            SaFlags::empty(),
            SigSet::empty(),
        );

        signal::sigaction(VM_STOP_SIGNAL, &action)
            .map(drop)
            .map_err(|err| err as i32)
    }) {
        Ok(()) => Ok(()),
        Err(errno) => Err(io::Error::from_raw_os_error(errno)),
    }
}

fn unblock_stop_signal() {
    let mut signals = SigSet::empty();
    signals.add(VM_STOP_SIGNAL);
    if let Err(err) = signal::pthread_sigmask(SigmaskHow::SIG_UNBLOCK, Some(&signals), None) {
        warn!(error = %err, "failed to unblock vm stop signal");
    }
}

fn lock_host_serial(serial: &Arc<Mutex<HostSerial>>) -> std::sync::MutexGuard<'_, HostSerial> {
    match serial.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    fn test_config(id: u8) -> VmConfig {
        VmConfig {
            id,
            name: format!("vm-{id}"),
            ssh_key: "ssh-ed25519 test".to_string(),
            memory_mib: 512,
        }
    }

    #[test]
    fn vm_config_derives_identity_from_id() {
        let config = test_config(7);

        assert_eq!(config.tap_name(), "tap7");
        assert_eq!(config.guest_ip(), "10.10.0.9");
        assert_eq!(config.guest_mac(), [0x02, 0x12, 0x34, 0x56, 0x78, 7]);
        assert_eq!(config.rootfs_path(), PathBuf::from("/tmp/vm-7/rootfs.ext4"));
        assert_eq!(config.memory_mib(), 512);
        assert_eq!(config.memory_size(), 512 << 20);
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
    fn handle_shutdown_rejects_when_manager_is_not_running() {
        let manager = VmManager::new();
        let handle = manager.handle().unwrap();

        let err = handle.shutdown().unwrap_err();

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
            .start_vm("test-vm".to_string(), "ssh-ed25519 test".to_string(), 512)
            .unwrap_err();

        assert_eq!(
            err.downcast_ref::<io::Error>().unwrap().kind(),
            ErrorKind::NotConnected
        );
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

    #[test]
    fn allocate_vm_config_returns_current_id_then_advances() {
        let registry = Arc::new(Mutex::new(VmRegistry::default()));

        let first =
            allocate_vm_config(&registry, "one".to_string(), "ssh test".to_string(), 512).unwrap();
        let second =
            allocate_vm_config(&registry, "two".to_string(), "ssh test".to_string(), 512).unwrap();

        assert_eq!(first.id(), 0);
        assert_eq!(second.id(), 1);
    }
}
