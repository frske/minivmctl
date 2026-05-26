use std::{
    borrow::{Borrow, BorrowMut},
    fs::{File, OpenOptions},
    io,
    ops::Deref,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    sync::Arc,
};

use libc::{IFNAMSIZ, TUN_F_CSUM, ifreq};
use nix::{ioctl_write_int_bad, ioctl_write_ptr_bad};
use virtio_bindings::bindings::{
    virtio_config::VIRTIO_F_VERSION_1,
    virtio_net::{
        VIRTIO_NET_F_CSUM, VIRTIO_NET_F_GUEST_CSUM, VIRTIO_NET_F_GUEST_ECN,
        VIRTIO_NET_F_GUEST_TSO4, VIRTIO_NET_F_GUEST_TSO6, VIRTIO_NET_F_GUEST_UFO,
        VIRTIO_NET_F_GUEST_USO4, VIRTIO_NET_F_GUEST_USO6, VIRTIO_NET_F_HOST_ECN,
        VIRTIO_NET_F_HOST_TSO4, VIRTIO_NET_F_HOST_TSO6, VIRTIO_NET_F_HOST_UFO,
        VIRTIO_NET_F_HOST_USO, VIRTIO_NET_F_MAC,
    },
};
use virtio_bindings::virtio_ids::VIRTIO_ID_NET;
use virtio_device::{VirtioConfig, VirtioDeviceActions, VirtioDeviceType, VirtioMmioDevice};
use virtio_queue::{DescriptorChain, Queue, QueueOwnedT, QueueT};
use vm_memory::{Address, Bytes, GuestMemoryBackend, GuestMemoryMmap};
use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};

use crate::virtio_common::{IrqTrigger, feature_bit, reset_config};

const TUN_F_TSO4: libc::c_uint = 0x02;
const TUN_F_TSO6: libc::c_uint = 0x04;
const TUN_F_TSO_ECN: libc::c_uint = 0x08;
const TUN_F_UFO: libc::c_uint = 0x10;
const TUN_F_USO4: libc::c_uint = 0x20;
const TUN_F_USO6: libc::c_uint = 0x40;

const NET_NUM_QUEUES: usize = 2;
const RX_INDEX: usize = 0;
const TX_INDEX: usize = 1;
const VNET_HDR_NUM_BUFFERS_OFFSET: usize = 10;

ioctl_write_ptr_bad!(tun_set_iff, 0x4004_54ca, ifreq);
ioctl_write_ptr_bad!(tun_set_vnet_hdr_size, 0x4004_54d8, libc::c_int);
ioctl_write_int_bad!(tun_set_offload, 0x4004_54d0);

pub struct Tap {
    file: File,
}

pub trait NetIo {
    fn probe(&self) -> TapOffload;
    fn set_offload(&self, _offload: libc::c_uint) -> io::Result<()> {
        Ok(())
    }
    fn readv<M>(&mut self, data: DescriptorChain<M>) -> io::Result<usize>
    where
        M: Deref + Clone,
        M::Target: GuestMemoryBackend;
    fn writev<M>(&mut self, data: DescriptorChain<M>) -> io::Result<usize>
    where
        M: Deref + Clone,
        M::Target: GuestMemoryBackend;
}

impl AsRawFd for Tap {
    fn as_raw_fd(&self) -> std::os::unix::prelude::RawFd {
        self.file.as_raw_fd()
    }
}

impl Tap {
    pub fn new(name: &str) -> io::Result<Self> {
        const DEVICE: &str = "/dev/net/tun";
        const FLAGS: i16 = (libc::IFF_TAP | libc::IFF_NO_PI | libc::IFF_VNET_HDR) as i16;

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(DEVICE)?;
        let mut cname = [0; IFNAMSIZ];

        if name.len() >= IFNAMSIZ {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "interface name too long",
            ));
        }

        cname[..name.len()].copy_from_slice(name.as_bytes());
        let mut ifr_name = [0 as libc::c_char; IFNAMSIZ];
        for (dst, src) in ifr_name.iter_mut().zip(cname) {
            *dst = src as libc::c_char;
        }
        let mut ifr = ifreq {
            ifr_name,
            // SAFETY: `ifreq` is a C ioctl struct. Zeroing the union before setting `ifru_flags`
            // matches the kernel API expectation for TUNSETIFF.
            ifr_ifru: unsafe { std::mem::zeroed() },
        };
        ifr.ifr_ifru.ifru_flags = FLAGS;

        // SAFETY: `file` is an open `/dev/net/tun` fd and `ifr` points to a valid initialized
        // `ifreq` for the duration of the ioctl call.
        unsafe { tun_set_iff(file.as_raw_fd(), &ifr) }.map_err(nix_to_io)?;

        Ok(Self { file })
    }

    pub fn set_vnet_hdr_size(&self, size: libc::c_int) -> io::Result<()> {
        // SAFETY: ioctl only reads `size`, which lives until the call returns.
        unsafe { tun_set_vnet_hdr_size(self.file.as_raw_fd(), &size) }.map_err(nix_to_io)?;

        Ok(())
    }

    fn readv<M>(&mut self, data: DescriptorChain<M>) -> io::Result<usize>
    where
        M: Deref + Clone,
        M::Target: GuestMemoryBackend,
    {
        let mut iovecs = Vec::new();
        let iov = chain_iovec(data, &mut iovecs, true)?;
        let iov_len = i32::try_from(iov.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too many iovecs"))?;
        // SAFETY: `iov` points to guest-memory slices that remain borrowed until `readv` returns;
        // `iov_len` is checked to fit the syscall ABI.
        let ret = unsafe { libc::readv(self.file.as_raw_fd(), iov.as_ptr(), iov_len) };
        if ret == -1 {
            return Err(io::Error::last_os_error());
        }

        Ok(ret as usize)
    }

    fn writev<M>(&mut self, data: DescriptorChain<M>) -> io::Result<usize>
    where
        M: Deref + Clone,
        M::Target: GuestMemoryBackend,
    {
        let mut iovecs = Vec::new();
        let iov = chain_iovec(data, &mut iovecs, false)?;
        let iov_len = i32::try_from(iov.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too many iovecs"))?;
        // SAFETY: `iov` points to guest-memory slices that remain borrowed until `writev` returns;
        // `iov_len` is checked to fit the syscall ABI.
        let ret = unsafe { libc::writev(self.file.as_raw_fd(), iov.as_ptr(), iov_len) };
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

    fn set_offload(&self, offload: libc::c_uint) -> io::Result<()> {
        Tap::set_offload(self, offload)
    }

    fn readv<M>(&mut self, data: DescriptorChain<M>) -> io::Result<usize>
    where
        M: Deref + Clone,
        M::Target: GuestMemoryBackend,
    {
        Tap::readv(self, data)
    }

    fn writev<M>(&mut self, data: DescriptorChain<M>) -> io::Result<usize>
    where
        M: Deref + Clone,
        M::Target: GuestMemoryBackend,
    {
        Tap::writev(self, data)
    }
}

fn chain_iovec<M>(
    chain: DescriptorChain<M>,
    buffer: &mut Vec<libc::iovec>,
    writable: bool,
) -> io::Result<&[libc::iovec]>
where
    M: Deref + Clone,
    M::Target: GuestMemoryBackend,
{
    buffer.clear();
    let chain_mem = chain.clone();
    let mem = chain_mem.memory();
    for desc in chain {
        if desc.is_write_only() != writable {
            return Err(io::Error::other(
                "descriptor permissions do not match operation",
            ));
        }
        buffer.push(libc::iovec {
            iov_base: mem
                .get_slice(desc.addr(), desc.len() as usize)
                .map_err(|_| io::Error::other("failed to get descriptor slice"))?
                .ptr_guard_mut()
                .as_ptr() as *mut _,
            iov_len: desc.len() as _,
        });
    }
    Ok(buffer.as_slice())
}

pub struct Net<I = Tap> {
    tap: I,
    cfg: VirtioConfig<Queue>,
    queue_events: [EventFd; NET_NUM_QUEUES],
    irq_trigger: IrqTrigger,
    mem: GuestMemoryMmap,
    pub activate_event: EventFd,
}

impl<I> Net<I>
where
    I: NetIo,
{
    pub fn with_tap(tap: I, mac: Option<[u8; 6]>, mem: GuestMemoryMmap) -> io::Result<Self> {
        let mut avail_features = tap.probe().virtio_features() | feature_bit(VIRTIO_F_VERSION_1);
        if mac.is_some() {
            avail_features |= feature_bit(VIRTIO_NET_F_MAC);
        }

        let queues = vec![
            Queue::new(256).map_err(io::Error::other)?,
            Queue::new(256).map_err(io::Error::other)?,
        ];
        let cfg = VirtioConfig::new(
            avail_features,
            queues,
            mac.map_or_else(Vec::new, |mac| mac.to_vec()),
        );
        let irq_trigger = IrqTrigger::new(Arc::clone(&cfg.interrupt_status))?;

        Ok(Self {
            tap,
            cfg,
            queue_events: [EventFd::new(EFD_NONBLOCK)?, EventFd::new(EFD_NONBLOCK)?],
            irq_trigger,
            mem,
            activate_event: EventFd::new(EFD_NONBLOCK)?,
        })
    }

    pub fn interrupt_event(&self) -> &EventFd {
        self.irq_trigger.event()
    }

    pub fn queues_events(&self) -> &[EventFd] {
        self.queue_events.as_slice()
    }

    pub fn process_tx(&mut self) -> io::Result<()> {
        let mem = self.mem.clone();
        let queue = &mut self.cfg.queues[TX_INDEX];
        let mut used_any = false;

        while let Some(desc) = queue.pop_descriptor_chain(&mem) {
            let desc_index = desc.head_index();
            let _ = self.tap.writev(desc);
            queue
                .add_used(&mem, desc_index, 0)
                .map_err(io::Error::other)?;
            used_any = true;
        }

        if used_any && queue.needs_notification(&mem).unwrap_or(true) {
            self.irq_trigger.trigger_vring()?;
        }
        Ok(())
    }

    pub fn process_rx(&mut self) -> io::Result<()> {
        let mem = self.mem.clone();
        let queue = &mut self.cfg.queues[RX_INDEX];
        let mut used_any = false;

        while let Some(desc) = queue.pop_descriptor_chain(&mem) {
            let desc_index = desc.head_index();
            match self.tap.readv(desc.clone()) {
                Ok(n) => {
                    write_vnet_num_buffers(desc);
                    queue
                        .add_used(&mem, desc_index, n as u32)
                        .map_err(io::Error::other)?;
                    used_any = true;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    queue.go_to_previous_position();
                    break;
                }
                Err(_) => {
                    queue
                        .add_used(&mem, desc_index, 0)
                        .map_err(io::Error::other)?;
                    used_any = true;
                }
            }
        }

        if used_any && queue.needs_notification(&mem).unwrap_or(true) {
            self.irq_trigger.trigger_vring()?;
        }

        Ok(())
    }
}

fn write_vnet_num_buffers<M>(chain: DescriptorChain<M>)
where
    M: Deref + Clone,
    M::Target: GuestMemoryBackend,
{
    let chain_mem = chain.clone();
    let mem = chain_mem.memory();
    let mut offset = VNET_HDR_NUM_BUFFERS_OFFSET;
    let num_buffers = 1u16.to_le_bytes();
    let mut bytes = num_buffers.as_slice();

    for desc in chain {
        if !desc.is_write_only() {
            continue;
        }
        let skip = offset.min(desc.len() as usize);
        offset -= skip;
        if offset == 0 {
            let to_copy = bytes.len().min(desc.len() as usize - skip);
            let _ = mem.write_slice(&bytes[..to_copy], desc.addr().unchecked_add(skip as u64));
            bytes = &bytes[to_copy..];
            if bytes.is_empty() {
                break;
            }
        }
    }
}

impl Net<Tap> {
    pub fn new(name: &str, mac: Option<[u8; 6]>, mem: GuestMemoryMmap) -> io::Result<Self> {
        let tap = Tap::new(name)?;
        tap.set_vnet_hdr_size(12)?;
        Net::with_tap(tap, mac, mem)
    }

    pub fn tap_raw_fd(&self) -> std::os::fd::RawFd {
        self.tap.as_raw_fd()
    }
}

impl<I> Borrow<VirtioConfig<Queue>> for Net<I> {
    fn borrow(&self) -> &VirtioConfig<Queue> {
        &self.cfg
    }
}

impl<I> BorrowMut<VirtioConfig<Queue>> for Net<I> {
    fn borrow_mut(&mut self) -> &mut VirtioConfig<Queue> {
        &mut self.cfg
    }
}

impl<I> VirtioDeviceType for Net<I> {
    fn device_type(&self) -> u32 {
        VIRTIO_ID_NET
    }
}

impl<I> VirtioDeviceActions for Net<I>
where
    I: NetIo,
{
    type E = io::Error;

    fn activate(&mut self) -> Result<(), Self::E> {
        self.tap
            .set_offload(TapOffload::from_virtio_features(self.cfg.driver_features).into_inner())?;
        self.activate_event.write(1)?;
        self.cfg.device_activated = true;
        Ok(())
    }

    fn reset(&mut self) -> Result<(), Self::E> {
        reset_config(&mut self.cfg);
        Ok(())
    }
}

impl<I> VirtioMmioDevice for Net<I> where I: NetIo + Send {}

pub struct TapOffload(libc::c_uint);

impl TapOffload {
    const AVAIL_MAPPING: [(libc::c_uint, u64); 7] = [
        (
            TUN_F_CSUM,
            feature_bit(VIRTIO_NET_F_GUEST_CSUM) | feature_bit(VIRTIO_NET_F_CSUM),
        ),
        (
            TUN_F_TSO4,
            feature_bit(VIRTIO_NET_F_GUEST_TSO4) | feature_bit(VIRTIO_NET_F_HOST_TSO4),
        ),
        (
            TUN_F_TSO6,
            feature_bit(VIRTIO_NET_F_GUEST_TSO6) | feature_bit(VIRTIO_NET_F_HOST_TSO6),
        ),
        (
            TUN_F_TSO_ECN,
            feature_bit(VIRTIO_NET_F_GUEST_ECN) | feature_bit(VIRTIO_NET_F_HOST_ECN),
        ),
        (
            TUN_F_UFO,
            feature_bit(VIRTIO_NET_F_GUEST_UFO) | feature_bit(VIRTIO_NET_F_HOST_UFO),
        ),
        (
            TUN_F_USO4,
            feature_bit(VIRTIO_NET_F_GUEST_USO4) | feature_bit(VIRTIO_NET_F_HOST_USO),
        ),
        (
            TUN_F_USO6,
            feature_bit(VIRTIO_NET_F_GUEST_USO6) | feature_bit(VIRTIO_NET_F_HOST_USO),
        ),
    ];
    const FEAT_MAPPING: [(u64, libc::c_uint); 7] = [
        (feature_bit(VIRTIO_NET_F_GUEST_CSUM), TUN_F_CSUM),
        (feature_bit(VIRTIO_NET_F_GUEST_TSO4), TUN_F_TSO4),
        (feature_bit(VIRTIO_NET_F_GUEST_TSO6), TUN_F_TSO6),
        (feature_bit(VIRTIO_NET_F_GUEST_ECN), TUN_F_TSO_ECN),
        (feature_bit(VIRTIO_NET_F_GUEST_UFO), TUN_F_UFO),
        (feature_bit(VIRTIO_NET_F_GUEST_USO4), TUN_F_USO4),
        (feature_bit(VIRTIO_NET_F_GUEST_USO6), TUN_F_USO6),
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
        if features & feature_bit(VIRTIO_NET_F_CSUM) == 0 {
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
        // SAFETY: ioctl receives an integer bitmask; no pointer aliasing/lifetime concerns.
        unsafe { tun_set_offload(self.file.as_raw_fd(), offload as _) }.map_err(nix_to_io)?;
        Ok(())
    }
}

fn nix_to_io(err: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(err as i32)
}
