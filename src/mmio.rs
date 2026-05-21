use std::{
    io,
    sync::{Arc, Mutex},
};

use virtio_device::VirtioMmioDevice;
use virtio_queue::Queue;

pub type MmioDevice = dyn VirtioMmioDevice<E = io::Error, Q = Queue> + Send;

pub struct MMIOTransport {
    device: Arc<Mutex<MmioDevice>>,
}

impl MMIOTransport {
    pub fn new(device: Arc<Mutex<MmioDevice>>) -> Self {
        Self { device }
    }

    pub fn bus_read(&mut self, offset: u64, data: &mut [u8]) {
        self.device.lock().unwrap().read(offset, data);
    }

    pub fn bus_write(&mut self, offset: u64, data: &[u8]) {
        self.device.lock().unwrap().write(offset, data);
    }
}
