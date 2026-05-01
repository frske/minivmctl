use std::{
    fs::File,
    io,
    mem::{transmute, zeroed},
    ops::Deref,
    os::fd::{AsRawFd, FromRawFd},
    sync::{atomic::AtomicU32, Arc},
};

use crate::virtio::{DescriptorChain, Queue, VirtioDevice};
use libc::{ifreq, IFNAMSIZ, TUN_F_CSUM};
use vm_memory::{GuestAddress, GuestMemoryBackend, GuestMemoryMmap};
use vm_superio::Trigger;
use vmm_sys_util::eventfd::{EventFd, EFD_NONBLOCK};
use vmm_sys_util::poll::PollContext;

const TUN_F_TSO4: libc::c_uint = 0x02;
const TUN_F_TSO6: libc::c_uint = 0x04;
const TUN_F_TSO_ECN: libc::c_uint = 0x08;
const TUN_F_UFO: libc::c_uint = 0x10;
const TUN_F_USO4: libc::c_uint = 0x20;
const TUN_F_USO6: libc::c_uint = 0x40;

const NET_NUM_QUEUES: usize = 2;
const TX_INDEX: usize = 0;
const RX_INDEX: usize = 1;
const VNET_HDR_NUM_BUFFERS_OFFSET: usize = 0;
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
}

impl Clone for IrqTrigger {
    fn clone(&self) -> Self {
        Self {
            irq_evt: self.irq_evt.try_clone().unwrap(),
        }
    }
}

impl IrqTrigger {
    fn new() -> io::Result<Self> {
        Ok(Self {
            irq_evt: EventFd::new(EFD_NONBLOCK)?,
        })
    }

    fn trigger(&self, _irq_type: IrqType) -> io::Result<()> {
        self.irq_evt.write(1)
    }
}

impl Trigger for IrqTrigger {
    type E = io::Error;

    fn trigger(&self) -> Result<(), Self::E> {
        self.irq_evt.write(1)
    }
}

#[derive(Default, Clone)]
struct ConfigSpace(Vec<u8>);

impl ConfigSpace {
    fn as_slice(&self) -> &[u8] {
        self.0.as_slice()
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        self.0.as_mut_slice()
    }
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

    fn unwrap(&self) -> GuestMemoryMmap {
        match self {
            Self::Activated { mem } => mem.clone(),
            Self::Inactive => panic!("device not activated"),
        }
    }
}

pub struct Tap {
    file: File,
    buffer: Vec<libc::iovec>,
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
            ifr_ifru: unsafe { zeroed() },
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

trait QueueUndoPop {
    fn undo_pop(&mut self) {}
}

impl QueueUndoPop for Queue {}

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
        let chain = self;
        while let Some(desc) = chain.next() {
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
        }
        Ok(buffer.as_slice())
    }
}

//I have so many questions
pub struct Net {
    tap: Tap,
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

unsafe impl Send for Net {}

impl VirtioDevice for Net {
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
            println!("write config space {len}");
        }
    }

    fn write_config(&mut self, _offset: u32, _data: &[u8]) {}

    fn activate(&mut self, mem: GuestMemoryMmap) -> io::Result<()> {
        if self.has_features(VIRTIO_RING_F_EVENT_IDX) {
            for queue in &mut self.queues {
                queue.notif_suppress = true;
            }
        }

        self.activate_event.write(1)?;
        self.state = DeviceState::Activated { mem };
        println!("activate done");
        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.state.is_activated()
    }
}

impl Net {
    pub fn new(name: &str, mac: Option<[u8; 6]>) -> io::Result<Self> {
        let tap = Tap::new(name)?;
        tap.set_vnet_hdr_size(12)?;

        let mut avail_features = tap.probe().virtio_features();
        if mac.is_some() {
            avail_features |= 1 << VIRTIO_NET_F_MAC;
        }

        Ok(Self {
            tap,
            avail_features,
            acked_features: 0,
            queues: [Queue::new(256), Queue::new(256)],
            queue_events: [EventFd::new(EFD_NONBLOCK)?, EventFd::new(EFD_NONBLOCK)?],
            irq_trigger: IrqTrigger::new()?,
            interrupt_status: Arc::new(AtomicU32::new(0)),
            config_space: ConfigSpace(mac.map_or_else(Vec::new, |mac| mac.to_vec())),
            state: DeviceState::Inactive,
            activate_event: EventFd::new(EFD_NONBLOCK)?,
        })
    }

    pub fn interrupt_event(&self) -> &EventFd {
        &self.irq_trigger.irq_evt
    }

    pub fn register_tap_readable(&self, poll: &PollContext<u8>, token: u8) -> io::Result<()> {
        poll.add(&self.tap, token)
            .map_err(|e| io::Error::from_raw_os_error(e.errno()))
    }

    pub fn process_tx(&mut self) -> io::Result<()> {
        let mem = self.state.unwrap();
        let queue = &mut self.queues[TX_INDEX];

        let mut try_notify = false;

        loop {
            let desc = match queue.pop(&mem) {
                Ok(Some(x)) => x,
                Ok(None) => break,
                Err(e) => {
                    println!("queue pop failed in process_tx: {:?}", e);
                    continue;
                }
            };

            let desc_index = desc.index;
            match self.tap.writev(desc) {
                Ok(_n) => {
                    queue.push(&mem, desc_index, 0);
                    try_notify = true;
                }
                Err(_e) => {
                    queue.push(&mem, desc_index, 0);
                    println!("tap writev failed");
                    try_notify = true;
                }
            };
        }

        if try_notify && queue.prepare_notify(&mem) {
            self.irq_trigger.trigger(IrqType::Vring)?;
        }
        Ok(())
    }

    pub fn process_rx(&mut self) -> io::Result<()> {
        let mem = self.state.unwrap();
        let queue = &mut self.queues[RX_INDEX];

        let mut try_notify = false;

        loop {
            let desc = match queue.pop(&mem) {
                Ok(Some(x)) => x,
                Ok(None) => break,
                Err(e) => {
                    println!("queue pop failed in process_rx: {:?}", e);
                    continue;
                }
            };
            let desc_index = desc.index;
            match self.tap.readv(desc.clone()) {
                Ok(n) => {
                    if let Err(_e) = desc.write_at(VNET_HDR_NUM_BUFFERS_OFFSET, &u16::to_le_bytes(1))
                    {
                        println!("failed to write vnet header num buffers");
                    }
                    queue.push(&mem, desc_index, n as u32);
                    try_notify = true;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    queue.undo_pop();
                    break;
                }
                Err(e) => {
                    println!("tap readv failed: {:?}", e);
                    queue.push(&mem, desc_index, 0);
                    try_notify = true;
                }
            };
        }

        if try_notify && queue.prepare_notify(&mem) {
            self.irq_trigger.trigger(IrqType::Vring)?;
        }

        Ok(())
    }
}

pub struct TapOffload(libc::c_uint);

impl TapOffload {
    const AVAIL_MAPPING: [(libc::c_uint, u64); 7] = [
        (TUN_F_CSUM, VIRTIO_NET_F_GUEST_CSUM | VIRTIO_NET_F_CSUM),
        (TUN_F_TSO4, VIRTIO_NET_F_GUEST_TSO4 | VIRTIO_NET_F_HOST_TSO4),
        (TUN_F_TSO6, VIRTIO_NET_F_GUEST_TSO6 | VIRTIO_NET_F_HOST_TSO6),
        (
            TUN_F_TSO_ECN,
            VIRTIO_NET_F_GUEST_ECN | VIRTIO_NET_F_HOST_ECN,
        ),
        (TUN_F_UFO, VIRTIO_NET_F_GUEST_UFO | VIRTIO_NET_F_HOST_UFO),
        (TUN_F_USO4, VIRTIO_NET_F_GUEST_USO4 | VIRTIO_NET_F_HOST_USO),
        (TUN_F_USO6, VIRTIO_NET_F_GUEST_USO6 | VIRTIO_NET_F_HOST_USO),
    ];
    const FEAT_MAPPING: [(u64, libc::c_uint); 7] = [
        (VIRTIO_NET_F_GUEST_CSUM, TUN_F_CSUM),
        (VIRTIO_NET_F_GUEST_TSO4, TUN_F_TSO4),
        (VIRTIO_NET_F_GUEST_TSO6, TUN_F_TSO6),
        (VIRTIO_NET_F_GUEST_ECN, TUN_F_TSO_ECN),
        (VIRTIO_NET_F_GUEST_UFO, TUN_F_UFO),
        (VIRTIO_NET_F_GUEST_USO4, TUN_F_USO4),
        (VIRTIO_NET_F_GUEST_USO6, TUN_F_USO6),
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
        if features & VIRTIO_NET_F_CSUM == 0 {
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
