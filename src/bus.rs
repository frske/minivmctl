use std::{
    ops::Range,
    ops::Sub,
    sync::{Arc, Mutex},
};

use virtio_bindings::virtio_mmio::VIRTIO_MMIO_CONFIG;

use crate::mmio::{MMIOTransport, MmioDevice};

const VIRTIO_MMIO_CONFIG_SPACE_SIZE: u64 = 0xf00;
pub const VIRTIO_MMIO_SIZE: u64 = VIRTIO_MMIO_CONFIG as u64 + VIRTIO_MMIO_CONFIG_SPACE_SIZE;

pub struct Bus<A, D> {
    devices: Vec<BusDevice<A, D>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusError {
    EmptyRange,
    Overlap,
    AddressOverflow,
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
        Self {
            devices: Vec::new(),
        }
    }

    pub fn insert(&mut self, range: Range<A>, device: Arc<Mutex<D>>) -> Result<(), BusError> {
        if range.is_empty() {
            return Err(BusError::EmptyRange);
        }

        let insert_idx = self
            .devices
            .partition_point(|d| d.range.start < range.start);

        if insert_idx > 0 && self.devices[insert_idx - 1].range.end > range.start {
            return Err(BusError::Overlap);
        }

        if insert_idx < self.devices.len() && range.end > self.devices[insert_idx].range.start {
            return Err(BusError::Overlap);
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

impl<A, D> Default for Bus<A, D>
where
    A: PartialOrd<A>,
{
    fn default() -> Self {
        Self::new()
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
    pub fn insert_serial(
        &mut self,
        address: u16,
        serial: Arc<Mutex<PIODevice>>,
    ) -> Result<(), BusError> {
        const SERIAL_PORT_SIZE: u16 = 0x8;
        let end = address
            .checked_add(SERIAL_PORT_SIZE)
            .ok_or(BusError::AddressOverflow)?;
        self.insert(address..end, serial)
    }
}

impl MMIOBus {
    pub fn insert_virtio(
        &mut self,
        address: u64,
        device: Arc<Mutex<MmioDevice>>,
    ) -> Result<(), BusError> {
        let device = MMIODevice::MMIOTransport(MMIOTransport::new(device));
        let end = address
            .checked_add(VIRTIO_MMIO_SIZE)
            .ok_or(BusError::AddressOverflow)?;
        self.insert(address..end, Arc::new(Mutex::new(device)))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default, Debug)]
    struct RecordingDevice {
        reads: Vec<(u16, usize)>,
        writes: Vec<(u16, Vec<u8>)>,
    }

    impl DeviceIO for RecordingDevice {
        type Offset = u16;

        fn read(&mut self, offset: Self::Offset, data: &mut [u8]) {
            self.reads.push((offset, data.len()));
            for (idx, byte) in data.iter_mut().enumerate() {
                *byte = offset as u8 + idx as u8;
            }
        }

        fn write(&mut self, offset: Self::Offset, data: &[u8]) {
            self.writes.push((offset, data.to_vec()));
        }
    }

    fn recording_device() -> Arc<Mutex<RecordingDevice>> {
        Arc::new(Mutex::new(RecordingDevice::default()))
    }

    // Adjacent ranges stay valid.
    #[test]
    fn insert_accepts_adjacent_ranges() {
        let mut bus = Bus::new();
        let dev1 = recording_device();
        let dev2 = recording_device();

        assert!(bus.insert(0u16..10, dev1).is_ok());
        assert!(bus.insert(10..20, dev2).is_ok());
    }

    // Empty range rejected.
    #[test]
    fn insert_rejects_empty_range() {
        let mut bus = Bus::new();

        assert!(bus.insert(5u16..5, recording_device()).is_err());
    }

    // Overlap with later range rejected.
    #[test]
    fn insert_rejects_overlap_with_next_range() {
        let mut bus = Bus::new();

        assert!(bus.insert(10u16..20, recording_device()).is_ok());
        assert!(bus.insert(0..15, recording_device()).is_err());
    }

    // Overlap with earlier range rejected.
    #[test]
    fn insert_rejects_overlap_with_previous_range() {
        let mut bus = Bus::new();

        assert!(bus.insert(0u16..10, recording_device()).is_ok());
        assert!(bus.insert(5..6, recording_device()).is_err());
    }

    // Lookup returns matching device.
    #[test]
    fn get_returns_matching_device() {
        let mut bus = Bus::new();
        let dev = recording_device();

        assert!(bus.insert(0x1000u16..0x1010, dev.clone()).is_ok());

        let (base, got) = bus.get(&0x1004).expect("device present");
        assert_eq!(*base, 0x1000);
        assert!(Arc::ptr_eq(got, &dev));
    }

    // Range start inclusive, end exclusive.
    #[test]
    fn get_respects_range_boundaries() {
        let mut bus = Bus::new();
        let dev = recording_device();

        assert!(bus.insert(0x1000u16..0x1010, dev).is_ok());
        assert!(bus.get(&0x1000).is_some());
        assert!(bus.get(&0x100f).is_some());
        assert!(bus.get(&0x1010).is_none());
    }

    // Read uses base-relative offset.
    #[test]
    fn read_routes_offset_relative_to_base() {
        let mut bus = Bus::new();
        let dev = recording_device();

        assert!(bus.insert(0x1000u16..0x1010, dev.clone()).is_ok());

        let mut data = [0u8; 4];
        assert!(bus.read(0x1003, &mut data));
        assert_eq!(data, [3, 4, 5, 6]);

        let guard = dev.lock().unwrap();
        assert_eq!(guard.reads, vec![(3, 4)]);
    }

    // Write uses base-relative offset.
    #[test]
    fn write_routes_offset_relative_to_base() {
        let mut bus = Bus::new();
        let dev = recording_device();

        assert!(bus.insert(0x2000u16..0x2010, dev.clone()).is_ok());

        assert!(bus.write(0x2007, &[1, 2, 3]));

        let guard = dev.lock().unwrap();
        assert_eq!(guard.writes, vec![(7, vec![1, 2, 3])]);
    }

    // Miss returns false.
    #[test]
    fn io_returns_false_on_miss() {
        let mut bus: Bus<u16, RecordingDevice> = Bus::new();

        let mut data = [0u8; 2];
        assert!(!bus.read(0x10, &mut data));
        assert!(!bus.write(0x10, &[1, 2]));
    }
}
