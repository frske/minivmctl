use std::{
    fs::File,
    io,
    mem::transmute,
    ops::Deref,
    os::fd::{AsRawFd, FromRawFd},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use crate::virtio::{DescriptorChain, Queue, VirtioDevice};
use libc::{IFNAMSIZ, TUN_F_CSUM, ifreq};
use vm_memory::{GuestAddress, GuestMemoryBackend, GuestMemoryMmap};
use vm_superio::Trigger;
use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};
use vmm_sys_util::poll::PollContext;

const TUN_F_TSO4: libc::c_uint = 0x02;
const TUN_F_TSO6: libc::c_uint = 0x04;
const TUN_F_TSO_ECN: libc::c_uint = 0x08;
const TUN_F_UFO: libc::c_uint = 0x10;
const TUN_F_USO4: libc::c_uint = 0x20;
const TUN_F_USO6: libc::c_uint = 0x40;

const IRQ_BIT_VRING: u32 = 0x1;
const IRQ_BIT_CONFIG: u32 = 0x2;

const NET_NUM_QUEUES: usize = 2;
const RX_INDEX: usize = 0;
const TX_INDEX: usize = 1;
const VNET_HDR_NUM_BUFFERS_OFFSET: usize = 10;
const VIRTIO_TYPE_NET: u32 = 1;
const VIRTIO_RING_F_EVENT_IDX: u64 = 29;

const VIRTIO_NET_F_CSUM: u64 = 0;
const VIRTIO_NET_F_GUEST_CSUM: u64 = 1;
const VIRTIO_NET_F_GUEST_TSO4: u64 = 7;
const VIRTIO_NET_F_GUEST_TSO6: u64 = 8;
const VIRTIO_NET_F_GUEST_ECN: u64 = 9;
const VIRTIO_NET_F_GUEST_UFO: u64 = 10;
const VIRTIO_NET_F_HOST_TSO4: u64 = 11;
const VIRTIO_NET_F_HOST_TSO6: u64 = 12;
const VIRTIO_NET_F_HOST_ECN: u64 = 13;
const VIRTIO_NET_F_HOST_UFO: u64 = 14;
const VIRTIO_NET_F_GUEST_USO4: u64 = 54;
const VIRTIO_NET_F_GUEST_USO6: u64 = 55;
const VIRTIO_NET_F_HOST_USO: u64 = 56;
const VIRTIO_NET_F_MAC: u64 = 5;

#[derive(Clone, Copy)]
enum IrqType {
    Config,
    Vring,
}

struct IrqTrigger {
    irq_evt: EventFd,
    irq_status: Arc<AtomicU32>,
}

impl Clone for IrqTrigger {
    fn clone(&self) -> Self {
        Self {
            irq_evt: self.irq_evt.try_clone().unwrap(),
            irq_status: Arc::clone(&self.irq_status),
        }
    }
}

impl IrqTrigger {
    fn new(irq_status: Arc<AtomicU32>) -> io::Result<Self> {
        Ok(Self {
            irq_evt: EventFd::new(EFD_NONBLOCK)?,
            irq_status,
        })
    }

    fn trigger(&self, irq_type: IrqType) -> io::Result<()> {
        let bit = match irq_type {
            IrqType::Config => IRQ_BIT_CONFIG,
            IrqType::Vring => IRQ_BIT_VRING,
        };
        self.irq_status.fetch_or(bit, Ordering::SeqCst);
        self.irq_evt.write(1)
    }
}

impl Trigger for IrqTrigger {
    type E = io::Error;

    fn trigger(&self) -> Result<(), Self::E> {
        self.trigger(IrqType::Config)
    }
}

#[derive(Default, Clone)]
struct ConfigSpace(Vec<u8>);

impl ConfigSpace {
    fn as_slice(&self) -> &[u8] {
        self.0.as_slice()
    }

    // fn as_mut_slice(&mut self) -> &mut [u8] {
    //     self.0.as_mut_slice()
    // }
}

enum DeviceState {
    Inactive,
    Activated { mem: GuestMemoryMmap },
}

impl Default for DeviceState {
    fn default() -> Self {
        Self::Inactive
    }
}

impl DeviceState {
    fn is_activated(&self) -> bool {
        matches!(self, Self::Activated { .. })
    }

    fn mem(&self) -> Option<GuestMemoryMmap> {
        match self {
            Self::Activated { mem } => Some(mem.clone()),
            Self::Inactive => None,
        }
    }
}

pub struct Tap {
    file: File,
    buffer: Vec<libc::iovec>,
}

unsafe impl Send for Tap {}

pub trait NetIo {
    fn probe(&self) -> TapOffload;
    fn readv<B: IntoIoVec>(&mut self, data: B) -> io::Result<usize>;
    fn writev<B: IntoIoVec>(&mut self, data: B) -> io::Result<usize>;
}

impl AsRawFd for Tap {
    fn as_raw_fd(&self) -> std::os::unix::prelude::RawFd {
        self.file.as_raw_fd()
    }
}

impl Tap {
    pub fn new(name: &str) -> io::Result<Self> {
        const DEVICE: &str = "/dev/net/tun\0";
        const FLAGS: i16 = (libc::IFF_TAP | libc::IFF_NO_PI | libc::IFF_VNET_HDR) as i16;
        const TUNSEITFF: libc::c_ulong = 0x4004_54ca;

        let fd = unsafe {
            libc::open(
                DEVICE.as_ptr() as *const _,
                libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };

        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let file = unsafe { File::from_raw_fd(fd) };

        let mut cname = [0; IFNAMSIZ];

        if name.as_bytes().len() >= IFNAMSIZ {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "interface name too long",
            ));
        }

        cname[..name.as_bytes().len()].copy_from_slice(name.as_bytes());

        let mut ifr = ifreq {
            ifr_name: unsafe { transmute::<[u8; IFNAMSIZ], [libc::c_char; IFNAMSIZ]>(cname) },
            ifr_ifru: unsafe { std::mem::zeroed() },
        };

        ifr.ifr_ifru.ifru_flags = FLAGS;

        if unsafe { libc::ioctl(fd, TUNSEITFF, &ifr) } < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            file,
            buffer: Vec::new(),
        })
    }

    pub fn set_vnet_hdr_size(&self, size: libc::c_int) -> io::Result<()> {
        const TUNSETVNETHDRSZ: libc::c_ulong = 0x4004_54d8;

        if unsafe { libc::ioctl(self.file.as_raw_fd(), TUNSETVNETHDRSZ, &size) } < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    fn readv<B: IntoIoVec>(&mut self, data: B) -> io::Result<usize> {
        let iov = data.into_iovec(&mut self.buffer, true)?;

        //use nix crate for these things
        // if iov.len() as i32 >= i32::MAX {
        //     return Err(i32::err)
        // }
        let ret = unsafe { libc::readv(self.file.as_raw_fd(), iov.as_ptr(), iov.len() as i32) };
        if ret == -1 {
            return Err(io::Error::last_os_error());
        }

        Ok(ret as usize)
    }

    fn writev<B: IntoIoVec>(&mut self, data: B) -> io::Result<usize> {
        let iov = data.into_iovec(&mut self.buffer, false)?;
        let ret = unsafe { libc::writev(self.file.as_raw_fd(), iov.as_ptr(), iov.len() as i32) };

        if ret == -1 {
            return Err(io::Error::last_os_error());
        }

        Ok(ret as usize)
    }
}

impl NetIo for Tap {
    fn probe(&self) -> TapOffload {
        Tap::probe(self)
    }

    fn readv<B: IntoIoVec>(&mut self, data: B) -> io::Result<usize> {
        Tap::readv(self, data)
    }

    fn writev<B: IntoIoVec>(&mut self, data: B) -> io::Result<usize> {
        Tap::writev(self, data)
    }
}

pub trait IntoIoVec {
    fn into_iovec(
        self,
        buffer: &mut Vec<libc::iovec>,
        write_only: bool,
    ) -> io::Result<&[libc::iovec]>;
}

impl<M> IntoIoVec for DescriptorChain<M>
where
    M: Deref + Copy,
    M::Target: GuestMemoryBackend,
{
    fn into_iovec(
        self,
        buffer: &mut Vec<libc::iovec>,
        write_only: bool,
    ) -> io::Result<&[libc::iovec]> {
        buffer.clear();
        let mut next_desc = Some(self);

        while let Some(desc) = next_desc {
            if (write_only && desc.is_write_only()) || (!write_only && !desc.is_write_only()) {
                buffer.push(libc::iovec {
                    iov_base: desc
                        .mem
                        .get_slice(GuestAddress(desc.addr), desc.len as usize)
                        .map_err(|_| {
                            io::Error::new(io::ErrorKind::Other, "Failed to get slice iovec")
                        })?
                        .ptr_guard_mut()
                        .as_ptr() as *mut _,
                    iov_len: desc.len as _,
                });
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Descriptor write_only bit not matches",
                ));
            }
            next_desc = desc.next();
        }
        Ok(buffer.as_slice())
    }
}

//I have so many questions
pub struct Net<I = Tap> {
    tap: I,
    avail_features: u64,
    acked_features: u64,
    queues: [Queue; NET_NUM_QUEUES],
    queue_events: [EventFd; NET_NUM_QUEUES],
    irq_trigger: IrqTrigger, //what is this dude
    interrupt_status: Arc<AtomicU32>,
    config_space: ConfigSpace,
    state: DeviceState,

    pub activate_event: EventFd,
}

unsafe impl<I: Send> Send for Net<I> {}

impl<I> VirtioDevice for Net<I>
where
    I: NetIo + Send,
{
    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn interrupt_status(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.interrupt_status)
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features;
    }

    fn device_type(&self) -> u32 {
        VIRTIO_TYPE_NET
    }

    fn queues(&self) -> &[Queue] {
        self.queues.as_slice()
    }

    fn queues_mut(&mut self) -> &mut [Queue] {
        self.queues.as_mut_slice()
    }

    fn queues_events(&self) -> &[EventFd] {
        self.queue_events.as_slice()
    }

    fn interrupt_trigger(&self) -> &dyn Trigger<E = io::Error> {
        //Send help
        &self.irq_trigger
    }

    fn read_config(&self, offset: u32, data: &mut [u8]) {
        if let Some(config_space_bytes) = self.config_space.as_slice().get(offset as usize..) {
            let len = config_space_bytes.len().min(data.len());
            data[..len].copy_from_slice(&config_space_bytes[..len]);
        }
    }

    fn write_config(&mut self, _offset: u32, _data: &[u8]) {}

    fn activate(&mut self, mem: GuestMemoryMmap) -> io::Result<()> {
        if self.has_features(1u64 << VIRTIO_RING_F_EVENT_IDX) {
            for queue in &mut self.queues {
                queue.notif_suppress = true;
            }
        }

        self.activate_event.write(1)?;
        self.state = DeviceState::Activated { mem };
        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.state.is_activated()
    }
}

impl<I> Net<I>
where
    I: NetIo,
{
    pub fn with_tap(tap: I, mac: Option<[u8; 6]>) -> io::Result<Self> {
        let mut avail_features = tap.probe().virtio_features();
        if mac.is_some() {
            avail_features |= 1 << VIRTIO_NET_F_MAC;
        }

        let interrupt_status = Arc::new(AtomicU32::new(0));

        Ok(Self {
            tap,
            avail_features,
            acked_features: 0,
            queues: [Queue::new(256), Queue::new(256)],
            queue_events: [EventFd::new(EFD_NONBLOCK)?, EventFd::new(EFD_NONBLOCK)?],
            irq_trigger: IrqTrigger::new(Arc::clone(&interrupt_status))?,
            interrupt_status,
            config_space: ConfigSpace(mac.map_or_else(Vec::new, |mac| mac.to_vec())),
            state: DeviceState::Inactive,
            activate_event: EventFd::new(EFD_NONBLOCK)?,
        })
    }

    pub fn interrupt_event(&self) -> &EventFd {
        &self.irq_trigger.irq_evt
    }

    pub fn process_tx(&mut self) -> io::Result<()> {
        let mem = self.state.mem().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "net device not activated")
        })?;
        let queue = &mut self.queues[TX_INDEX];

        let mut try_notify = false;

        loop {
            let desc = match queue.pop(&mem) {
                Ok(Some(x)) => x,
                Ok(None) | Err(_) => break,
            };

            let desc_index = desc.index;
            let write_succeeded = self.tap.writev(desc).is_ok();
            queue.push(&mem, desc_index, 0);
            if !write_succeeded {
                // Tap write errors are non-fatal; we still complete the descriptor.
            }
            try_notify = true;
        }

        if try_notify && queue.prepare_notify(&mem) {
            self.irq_trigger.trigger(IrqType::Vring)?;
        }
        Ok(())
    }

    pub fn process_rx(&mut self) -> io::Result<()> {
        let mem = self.state.mem().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "net device not activated")
        })?;
        let queue = &mut self.queues[RX_INDEX];

        let mut try_notify = false;

        loop {
            let desc = match queue.pop(&mem) {
                Ok(Some(x)) => x,
                Ok(None) => break,
                Err(_) => continue,
            };
            let desc_index = desc.index;

            match self.tap.readv(desc.clone()) {
                Ok(n) => {
                    write_vnet_num_buffers(&desc);
                    queue.push(&mem, desc_index, n as u32);
                    try_notify = true;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    queue.undo_pop();
                    break;
                }
                Err(_) => {
                    queue.push(&mem, desc_index, 0);
                    try_notify = true;
                }
            }
        }

        if try_notify && queue.prepare_notify(&mem) {
            self.irq_trigger.trigger(IrqType::Vring)?;
        }

        Ok(())
    }
}

fn write_vnet_num_buffers<M>(desc: &DescriptorChain<M>)
where
    M: Deref + Clone,
    M::Target: GuestMemoryBackend,
{
    let _ = desc.clone().write_at(VNET_HDR_NUM_BUFFERS_OFFSET, &u16::to_le_bytes(1));
}

impl Net<Tap> {
    pub fn new(name: &str, mac: Option<[u8; 6]>) -> io::Result<Self> {
        let tap = Tap::new(name)?;
        tap.set_vnet_hdr_size(12)?;
        Net::with_tap(tap, mac)
    }

    pub fn register_tap_readable(&self, poll: &PollContext<u8>, token: u8) -> io::Result<()> {
        poll.add(&self.tap, token)
            .map_err(|e| io::Error::from_raw_os_error(e.errno()))
    }
}

pub struct TapOffload(libc::c_uint);

impl TapOffload {
    const AVAIL_MAPPING: [(libc::c_uint, u64); 7] = [
        (
            TUN_F_CSUM,
            (1 << VIRTIO_NET_F_GUEST_CSUM) | (1 << VIRTIO_NET_F_CSUM),
        ),
        (
            TUN_F_TSO4,
            (1 << VIRTIO_NET_F_GUEST_TSO4) | (1 << VIRTIO_NET_F_HOST_TSO4),
        ),
        (
            TUN_F_TSO6,
            (1 << VIRTIO_NET_F_GUEST_TSO6) | (1 << VIRTIO_NET_F_HOST_TSO6),
        ),
        (
            TUN_F_TSO_ECN,
            (1 << VIRTIO_NET_F_GUEST_ECN) | (1 << VIRTIO_NET_F_HOST_ECN),
        ),
        (
            TUN_F_UFO,
            (1 << VIRTIO_NET_F_GUEST_UFO) | (1 << VIRTIO_NET_F_HOST_UFO),
        ),
        (
            TUN_F_USO4,
            (1 << VIRTIO_NET_F_GUEST_USO4) | (1 << VIRTIO_NET_F_HOST_USO),
        ),
        (
            TUN_F_USO6,
            (1 << VIRTIO_NET_F_GUEST_USO6) | (1 << VIRTIO_NET_F_HOST_USO),
        ),
    ];
    const FEAT_MAPPING: [(u64, libc::c_uint); 7] = [
        (1 << VIRTIO_NET_F_GUEST_CSUM, TUN_F_CSUM),
        (1 << VIRTIO_NET_F_GUEST_TSO4, TUN_F_TSO4),
        (1 << VIRTIO_NET_F_GUEST_TSO6, TUN_F_TSO6),
        (1 << VIRTIO_NET_F_GUEST_ECN, TUN_F_TSO_ECN),
        (1 << VIRTIO_NET_F_GUEST_UFO, TUN_F_UFO),
        (1 << VIRTIO_NET_F_GUEST_USO4, TUN_F_USO4),
        (1 << VIRTIO_NET_F_GUEST_USO6, TUN_F_USO6),
    ];

    pub fn virtio_features(&self) -> u64 {
        if self.0 & TUN_F_CSUM == 0 {
            return 0;
        }

        let mut features = 0;

        for (tun, virtio) in Self::AVAIL_MAPPING.iter() {
            if self.0 & tun != 0 {
                features |= *virtio;
            }
        }
        features
    }

    pub fn from_virtio_features(features: u64) -> Self {
        if features & (1 << VIRTIO_NET_F_CSUM) == 0 {
            return Self(0);
        }

        let mut tun = 0;

        for (virtio, t) in Self::FEAT_MAPPING.iter() {
            if features & virtio != 0 {
                tun |= *t;
            }
        }
        Self(tun)
    }

    pub fn into_inner(self) -> libc::c_uint {
        self.0
    }
}

impl Tap {
    pub fn probe(&self) -> TapOffload {
        let mut features: libc::c_uint = 0;
        const PROBE_LIST: &[libc::c_uint] = &[
            0,
            TUN_F_TSO4,
            TUN_F_TSO6,
            TUN_F_TSO_ECN,
            TUN_F_USO4,
            TUN_F_USO6,
        ];

        for p in PROBE_LIST {
            let probe = *p | TUN_F_CSUM;
            if self.set_offload(probe).is_ok() {
                features |= probe;
            }
        }
        TapOffload(features)
    }

    pub fn set_offload(&self, offload: libc::c_uint) -> io::Result<()> {
        const TUNSETOFFLOAD: libc::c_ulong = 0x4004_54d0;
        if unsafe { libc::ioctl(self.file.as_raw_fd(), TUNSETOFFLOAD, offload) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::VirtioDevice;
    use std::{
        collections::VecDeque,
        io,
        sync::{Arc, Mutex, atomic::Ordering},
    };
    use vm_memory::{Address, ByteValued, Bytes, GuestAddress, GuestMemoryMmap};

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Descriptor {
        addr: u64,
        len: u32,
        flags: u16,
        next: u16,
    }

    unsafe impl ByteValued for Descriptor {}

    #[derive(Clone, Copy)]
    enum Action {
        Ok(usize),
        WouldBlock,
    }

    impl Action {
        fn into_result(self) -> io::Result<usize> {
            match self {
                Self::Ok(n) => Ok(n),
                Self::WouldBlock => Err(io::Error::from(io::ErrorKind::WouldBlock)),
            }
        }
    }

    struct FakeTap {
        probe_features: u64,
        reads: VecDeque<Action>,
        writes: VecDeque<Action>,
        read_calls: usize,
        write_calls: usize,
    }

    #[derive(Clone)]
    struct SharedTap(Arc<Mutex<FakeTap>>);

    impl SharedTap {
        fn new(tap: FakeTap) -> Self {
            Self(Arc::new(Mutex::new(tap)))
        }
    }

    impl FakeTap {
        fn new() -> Self {
            Self {
                probe_features: 0,
                reads: VecDeque::new(),
                writes: VecDeque::new(),
                read_calls: 0,
                write_calls: 0,
            }
        }

        fn with_probe_features(mut self, features: u64) -> Self {
            self.probe_features = features;
            self
        }

        fn push_read(&mut self, action: Action) {
            self.reads.push_back(action);
        }

        fn push_write(&mut self, action: Action) {
            self.writes.push_back(action);
        }
    }

    impl NetIo for FakeTap {
        fn probe(&self) -> TapOffload {
            TapOffload::from_virtio_features(self.probe_features)
        }

        fn readv<B: IntoIoVec>(&mut self, _data: B) -> io::Result<usize> {
            self.read_calls += 1;
            self.reads
                .pop_front()
                .unwrap_or(Action::Ok(0))
                .into_result()
        }

        fn writev<B: IntoIoVec>(&mut self, _data: B) -> io::Result<usize> {
            self.write_calls += 1;
            self.writes
                .pop_front()
                .unwrap_or(Action::Ok(0))
                .into_result()
        }
    }

    impl NetIo for SharedTap {
        fn probe(&self) -> TapOffload {
            self.0.lock().unwrap().probe()
        }

        fn readv<B: IntoIoVec>(&mut self, data: B) -> io::Result<usize> {
            self.0.lock().unwrap().readv(data)
        }

        fn writev<B: IntoIoVec>(&mut self, data: B) -> io::Result<usize> {
            self.0.lock().unwrap().writev(data)
        }
    }

    fn write_descriptor(
        mem: &GuestMemoryMmap<()>,
        table: GuestAddress,
        index: u16,
        desc: Descriptor,
    ) {
        mem.write_obj(desc, table.unchecked_add(u64::from(index) * 16))
            .unwrap();
    }

    fn setup_queue(mem: &GuestMemoryMmap<()>, queue: &mut Queue, flags: u16) {
        queue.size = 8;
        queue.ready = true;
        queue.desc_table = GuestAddress(0x1000);
        queue.avail_ring = GuestAddress(0x2000);
        queue.used_ring = GuestAddress(0x3000);

        write_descriptor(
            mem,
            queue.desc_table,
            0,
            Descriptor {
                addr: 0x4000,
                len: 64,
                flags,
                next: 0,
            },
        );
        mem.write_obj(0u16, queue.avail_ring).unwrap();
        mem.write_obj(1u16, queue.avail_ring.unchecked_add(2))
            .unwrap();
        mem.write_obj(0u16, queue.avail_ring.unchecked_add(4))
            .unwrap();
    }

    fn setup_net(tap: SharedTap) -> (Net<SharedTap>, SharedTap, GuestMemoryMmap<()>) {
        let mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x20_000)]).unwrap();
        let net: Net<SharedTap> = Net::with_tap(tap.clone(), None).unwrap();
        (net, tap, mem)
    }

    // WouldBlock rewinds queue.
    #[test]
    fn process_rx_wouldblock_undoes_pop() {
        let mut tap = FakeTap::new();
        tap.push_read(Action::WouldBlock);
        let tap = SharedTap::new(tap);

        let (mut net, tap, mem) = setup_net(tap);
        setup_queue(&mem, &mut net.queues_mut()[0], 2);
        net.activate(mem.clone()).unwrap();

        net.process_rx().unwrap();

        let queue = &net.queues()[0];
        assert_eq!(queue.next_avail.0, 0);
        assert_eq!(queue.next_used.0, 0);
        assert_eq!(net.interrupt_status().load(Ordering::SeqCst), 0);
        assert_eq!(tap.0.lock().unwrap().read_calls, 1);
    }

    // RX success writes header and used ring.
    #[test]
    fn process_rx_success_writes_header_and_used_entry() {
        let mut tap = FakeTap::new();
        tap.push_read(Action::Ok(64));
        let tap = SharedTap::new(tap);

        let (mut net, tap, mem) = setup_net(tap);
        setup_queue(&mem, &mut net.queues_mut()[0], 2);
        net.activate(mem.clone()).unwrap();

        net.process_rx().unwrap();

        let queue = &net.queues()[0];
        let num_buffers: u16 = mem.read_obj(GuestAddress(0x4000 + 10)).unwrap();
        let used_id: u32 = mem.read_obj(queue.used_ring.unchecked_add(4)).unwrap();
        let used_len: u32 = mem.read_obj(queue.used_ring.unchecked_add(8)).unwrap();

        assert_eq!(num_buffers, 1);
        assert_eq!(used_id, 0);
        assert_eq!(used_len, 64);
        assert_eq!(queue.next_used.0, 1);
        assert_eq!(net.interrupt_status().load(Ordering::SeqCst) & 1, 1);
        assert_eq!(tap.0.lock().unwrap().read_calls, 1);
    }

    // TX success updates used ring.
    #[test]
    fn process_tx_success_pushes_used_entry() {
        let mut tap = FakeTap::new();
        tap.push_write(Action::Ok(32));
        let tap = SharedTap::new(tap);

        let (mut net, tap, mem) = setup_net(tap);
        setup_queue(&mem, &mut net.queues_mut()[1], 0);
        net.activate(mem.clone()).unwrap();

        net.process_tx().unwrap();

        let queue = &net.queues()[1];
        let used_id: u32 = mem.read_obj(queue.used_ring.unchecked_add(4)).unwrap();
        let used_len: u32 = mem.read_obj(queue.used_ring.unchecked_add(8)).unwrap();

        assert_eq!(used_id, 0);
        assert_eq!(used_len, 0);
        assert_eq!(queue.next_used.0, 1);
        assert_eq!(net.interrupt_status().load(Ordering::SeqCst) & 1, 1);
        assert_eq!(tap.0.lock().unwrap().write_calls, 1);
    }

    // TX WouldBlock still completes used entry.
    #[test]
    fn process_tx_wouldblock_pushes_used_entry() {
        let mut tap = FakeTap::new();
        tap.push_write(Action::WouldBlock);
        let tap = SharedTap::new(tap);

        let (mut net, tap, mem) = setup_net(tap);
        setup_queue(&mem, &mut net.queues_mut()[1], 0);
        net.activate(mem.clone()).unwrap();

        net.process_tx().unwrap();

        let queue = &net.queues()[1];
        let used_id: u32 = mem.read_obj(queue.used_ring.unchecked_add(4)).unwrap();
        let used_len: u32 = mem.read_obj(queue.used_ring.unchecked_add(8)).unwrap();

        assert_eq!(used_id, 0);
        assert_eq!(used_len, 0);
        assert_eq!(queue.next_used.0, 1);
        assert_eq!(net.interrupt_status().load(Ordering::SeqCst) & 1, 1);
        assert_eq!(tap.0.lock().unwrap().write_calls, 1);
    }

    // Event idx flips notif suppress.
    #[test]
    fn activate_with_event_idx_enables_notif_suppress() {
        let (mut net, _tap, mem) = setup_net(SharedTap::new(FakeTap::new().with_probe_features(0)));
        net.set_acked_features(1u64 << 29);
        assert_ne!(net.acked_features() & (1u64 << 29), 0);

        net.activate(mem).unwrap();

        assert!(net.queues()[0].notif_suppress);
        assert!(net.queues()[1].notif_suppress);
        assert_eq!(net.activate_event.read().unwrap(), 1);
    }

    // No event idx, no suppress.
    #[test]
    fn activate_without_event_idx_leaves_notif_suppress_off() {
        let (mut net, _tap, mem) = setup_net(SharedTap::new(FakeTap::new()));

        net.activate(mem).unwrap();

        assert!(!net.queues()[0].notif_suppress);
        assert!(!net.queues()[1].notif_suppress);
    }

    // Virtio bits map to TAP bits.
    #[test]
    fn tap_offload_maps_tun_to_virtio_features() {
        let offload = TapOffload::from_virtio_features((1 << 0) | (1 << 1) | (1 << 7) | (1 << 11));
        let virtio = offload.virtio_features();

        assert_ne!(virtio & (1 << 0), 0);
        assert_ne!(virtio & (1 << 1), 0);
        assert_ne!(virtio & (1 << 7), 0);
        assert_ne!(virtio & (1 << 11), 0);
    }

    // Checksum bit required.
    #[test]
    fn tap_offload_requires_checksum_bit() {
        assert_eq!(TapOffload::from_virtio_features(1 << 7).into_inner(), 0);
        assert_eq!(TapOffload::from_virtio_features(0).virtio_features(), 0);
    }

    // Feature roundtrip stays stable.
    #[test]
    fn tap_offload_roundtrips_supported_features() {
        let features = (1 << 0) | (1 << 1) | (1 << 7) | (1 << 54) | (1 << 55);
        let offload = TapOffload::from_virtio_features(features);

        assert_eq!(offload.into_inner(), libc::TUN_F_CSUM | 0x02 | 0x20 | 0x40);
    }

    // I/O vec honors perms.
    #[test]
    fn descriptor_chain_into_iovec_matches_permissions() {
        let mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x10_000)]).unwrap();
        let table = GuestAddress(0x1000);

        write_descriptor(
            &mem,
            table,
            0,
            Descriptor {
                addr: 0x4000,
                len: 4,
                flags: 0,
                next: 0,
            },
        );

        let chain = DescriptorChain::new(&mem, table, 2, 0).unwrap();
        let mut buf = Vec::new();

        let iov = chain.into_iovec(&mut buf, false).unwrap();
        assert_eq!(iov.len(), 1);

        write_descriptor(
            &mem,
            table,
            0,
            Descriptor {
                addr: 0x5000,
                len: 4,
                flags: 2,
                next: 0,
            },
        );
        let chain = DescriptorChain::new(&mem, table, 2, 0).unwrap();
        let iov = chain.into_iovec(&mut buf, true).unwrap();
        assert_eq!(iov.len(), 1);
    }

    // Wrong perms rejected.
    #[test]
    fn descriptor_chain_into_iovec_rejects_mismatch() {
        let mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x10_000)]).unwrap();
        let table = GuestAddress(0x1000);

        write_descriptor(
            &mem,
            table,
            0,
            Descriptor {
                addr: 0x4000,
                len: 4,
                flags: 2,
                next: 0,
            },
        );

        let chain = DescriptorChain::new(&mem, table, 1, 0).unwrap();
        let mut buf = Vec::new();

        assert!(chain.into_iovec(&mut buf, false).is_err());
    }
}
