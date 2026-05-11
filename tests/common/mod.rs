use std::{fmt::Debug, sync::{Arc, Mutex}};

use minivmctl::bus::DeviceIO;
use vm_memory::{ByteValued, Bytes, GuestAddress, GuestMemoryMmap};

#[derive(Default, Debug)]
pub struct RecordingDevice {
    pub reads: Vec<(u16, usize)>,
    pub writes: Vec<(u16, Vec<u8>)>,
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

pub fn recording_device() -> Arc<Mutex<RecordingDevice>> {
    Arc::new(Mutex::new(RecordingDevice::default()))
}

pub fn guest_memory(size: usize) -> GuestMemoryMmap<()> {
    GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), size)]).unwrap()
}

pub fn write_obj<M, T>(mem: &M, value: T, addr: GuestAddress)
where
    M: Bytes<GuestAddress>,
    T: ByteValued,
    <M as Bytes<GuestAddress>>::E: Debug,
{
    mem.write_obj(value, addr).unwrap();
}
