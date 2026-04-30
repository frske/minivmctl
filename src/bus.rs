use std::{
    ops::Sub,
    ops::Range,
    process::Output,
    sync::{Arc, Mutex},
};

use vm_memory::{GuestMemoryMmap, address};

use crate::{mmio::MMIOTransport, serial, virtio::VirtioDevice};

pub struct Bus<A, D> {
    devices: Vec<BusDevice<A, D>>,
}

struct BusDevice<A, D> {
    range: Range<A>,
    device: Arc<Mutex<D>>,
}

//rewrite with total ordering Ord
// use better error type
impl<A, D> Bus<A, D>
where
    A: PartialOrd<A>,
{
    pub fn new() -> Self {
        Self { devices: Vec::new() }
    }

    pub fn insert(&mut self, range: Range<A>, device: Arc<Mutex<D>>) -> Result<(), ()> {
        if range.is_empty() {
            return Err(());
        }

        let insert_idx = self
            .devices
            .partition_point(|d| d.range.start < range.start);

        if insert_idx < self.devices.len() && range.end > self.devices[insert_idx].range.start {
            return Err(());
        }

        self.devices.insert(insert_idx, BusDevice { range, device });

        Ok(())
    }

    pub fn get(&self, addr: &A) -> Option<(&A, &Arc<Mutex<D>>)> {
        self.devices
            .binary_search_by(|d| {
                if d.range.contains(addr) {
                    std::cmp::Ordering::Equal
                } else if &d.range.start > addr {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Less
                }
            })
            .ok()
            .map(|idx| (&self.devices[idx].range.start, &self.devices[idx].device))
    }
}

pub trait DeviceIO {
    type Offset;
    fn read(&mut self, offset: Self::Offset, data: &mut [u8]);
    fn write(&mut self, offset: Self::Offset, data: &[u8]);
}

impl<A, D, O> Bus<A, D>
where
    A: PartialOrd<A> + Sub<A, Output = O> + Copy + std::fmt::LowerHex,
    D: DeviceIO<Offset = O>,
{
    // this can be a not mutable self, but just a reference to itself
    #[inline]
    pub fn read(&mut self, addr: A, data: &mut [u8]) -> bool {
        if let Some((base, device)) = self.get(&addr) {
            let mut device = device.lock().unwrap();
            let offset = addr - *base;
            device.read(offset, data);
            true
        } else {
            false
        }
    }

    #[inline]
    pub fn write(&mut self, addr: A, data: &[u8]) -> bool {
        if let Some((base, device)) = self.get(&addr) {
            let mut device = device.lock().unwrap();
            let offset = addr - *base;
            device.write(offset, data);
            true
        } else {
            false
        }
    }
}

pub type PIOBus = Bus<u16, PIODevice>;
pub type MMIOBus = Bus<u64, MMIODevice>;
type SerialDevice = ();

#[non_exhaustive]
pub enum PIODevice {
    Serial(SerialDevice),
}

#[non_exhaustive]
pub enum MMIODevice {
    MMIOTransport(MMIOTransport),
}

impl PIOBus {
    pub fn insert_serial(&mut self, address: u16, serial: Arc<Mutex<PIODevice>>) -> Result<(), ()> {
        const SERIAL_PORT_SIZE: u16 = 0x8;
        self.insert(address..address + SERIAL_PORT_SIZE, serial)
    }
}

impl MMIOBus {
    pub fn insert_virtio(
        &mut self,
        address: u64,
        device: Arc<Mutex<dyn VirtioDevice>>,
        mem: GuestMemoryMmap,
    ) -> Result<(), ()> {
        const MMIO_SIZE: u64 = 0x1000;
        let device = MMIODevice::MMIOTransport(MMIOTransport::new(device, mem));
        self.insert(address..address + MMIO_SIZE, Arc::new(Mutex::new(device)))
    }
}

impl DeviceIO for PIODevice {
    type Offset = u16;

    fn read(&mut self, _offset: Self::Offset, _data: &mut [u8]) {}

    fn write(&mut self, _offset: Self::Offset, _data: &[u8]) {}
}

impl DeviceIO for MMIODevice {
    type Offset = u64;

    fn read(&mut self, offset: Self::Offset, data: &mut [u8]) {
        match self {
            MMIODevice::MMIOTransport(mmio) => mmio.bus_read(offset, data),
        }
    }

    fn write(&mut self, offset: Self::Offset, data: &[u8]) {
        match self {
            MMIODevice::MMIOTransport(mmio) => mmio.bus_write(offset, data),
        }
    }
}
