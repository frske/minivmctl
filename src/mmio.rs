const MMIO_MAGIC_VALUE: u32 = 0x7472_6976;
const MMIO_VERSION: u32 = 2;
const VENDOR_ID: u32 = 0;

const CONFIG_SPACE_OFFSET: u64 = 0x100;

use std::{
    num::Wrapping,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
};

use crate::virtio::{Queue, VirtioDevice};
use tracing::{debug, warn};
use vm_memory::GuestAddress;
use vm_memory::GuestMemoryMmap;

mod device_status {
    pub const INIT: u32 = 0;
    pub const ACKNOWLEDGE: u32 = 1;
    pub const DRIVER: u32 = 2;
    pub const FAILED: u32 = 128;
    pub const FEATURES_OK: u32 = 8;
    pub const DRIVER_OK: u32 = 4;
    pub const DEVICE_NEEDS_RESET: u32 = 64;
}

pub struct MMIOTransport {
    device: Arc<Mutex<dyn VirtioDevice>>,
    features_select: u32,
    acked_features_select: u32,
    queue_select: u32,
    device_status: u32,
    interrupt_status: Arc<AtomicU32>,
    config_generation: Wrapping<u32>,
    mem: GuestMemoryMmap,
}

impl MMIOTransport {
    pub fn new(device: Arc<Mutex<dyn VirtioDevice>>, mem: GuestMemoryMmap) -> Self {
        let interrupt_status = device.lock().unwrap().interrupt_status();

        Self {
            device,
            features_select: 0,
            acked_features_select: 0,
            queue_select: 0,
            device_status: 0,
            interrupt_status,
            config_generation: Wrapping(0),
            mem,
        }
    }
}

impl MMIOTransport {
    #[inline]
    // fn with_queue<F: FnOnce(&Queue) -> O, O>(&self, f: F) -> Option<O> {
    // }
    fn with_queue<F, O>(&self, f: F) -> Option<O>
    where
        F: FnOnce(&Queue) -> O,
    {
        self.device
            .lock()
            .unwrap()
            .queues()
            .get(self.queue_select as usize)
            .map(f)

        // let guard = self.device.lock().unwrap();
        // let queue = guard.queues().get(self.queue_select as usize)?;
        // Some(f(queue))
    }

    #[inline]
    fn with_queue_mut<F, O>(&mut self, f: F) -> Option<O>
    where
        F: FnOnce(&mut Queue) -> O,
    {
        self.device
            .lock()
            .unwrap()
            .queues_mut()
            .get_mut(self.queue_select as usize)
            .map(f)
    }

    #[inline]
    const fn check_device_status(&self, set: u32, clear: u32) -> bool {
        // self.device_status & (set | clear) == set
        (self.device_status & set) == set && (self.device_status & clear) == 0
    }

    fn update_queue_field<F>(&mut self, f: F)
    where
        F: FnOnce(&mut Queue),
    {
        if self.check_device_status(
            device_status::FEATURES_OK,
            device_status::DRIVER_OK | device_status::FAILED,
        ) {
            self.with_queue_mut(f);
        }
    }

    fn set_device_status(&mut self, status: u32) {
        use device_status::*;

        let newly_set = status & !self.device_status;

        match newly_set {
            ACKNOWLEDGE if self.device_status == INIT => {
                self.device_status = status;
            }
            DRIVER if self.device_status == ACKNOWLEDGE => {
                self.device_status = status;
            }
            FEATURES_OK if self.device_status == (ACKNOWLEDGE | DRIVER) => {
                self.device_status = status;
            }
            DRIVER_OK if self.device_status == (ACKNOWLEDGE | DRIVER | FEATURES_OK) => {
                self.device_status = status;
                self.try_activate_device();
            }
            _ if (status & FAILED) != 0 => {
                self.device_status |= device_status::FAILED;
                // todo: notify backend driver to stop the device
            }
            _ if status == 0 => {
                self.reset_or_fail();
            }
            _ => (),
        }
    }

    fn try_activate_device(&mut self) {
        let mut device = self.device.lock().unwrap();
        if device.is_activated() {
            return;
        }
        let all_queues_valid = device.queues().iter().all(|q| q.is_valid(&self.mem));
        if !all_queues_valid {
            warn!("device activation requested before all queues are valid");
            return;
        }
        if let Err(err) = device.activate(self.mem.clone()) {
            self.device_status |= device_status::DEVICE_NEEDS_RESET;
            let _ = device.interrupt_trigger().trigger(); //there was a trigger type here IrqType::Config
            //no clue what is supposed ot happen here, probably just trigger() with no type+
            warn!(error = %err, "device activation failed");
            return;
        }

        debug!("device activated");
    }

    fn reset_or_fail(&mut self) {
        let mut device = self.device.lock().unwrap();
        if device.is_activated() && device.reset().is_none() {
            self.device_status |= device_status::FAILED;
            warn!("device reset failed");
        }
        if self.device_status & device_status::FAILED == 0 {
            self.features_select = 0;
            self.acked_features_select = 0;
            self.queue_select = 0;
            self.interrupt_status.store(0, Ordering::SeqCst);
            self.device_status = device_status::INIT;

            for queue in device.queues_mut() {
                *queue = Queue::new(queue.max_size);
            }
        }
    }

    pub fn bus_read(&mut self, offset: u64, data: &mut [u8]) {
        if offset <= 0xff && data.len() == std::mem::size_of::<u32>() {
            let v = match offset {
                0x00 => MMIO_MAGIC_VALUE,
                0x04 => MMIO_VERSION,
                0x08 => self.device.lock().unwrap().device_type(),
                0x0c => VENDOR_ID,
                0x10 => self.features_for_page(),
                0x34 => self.with_queue(|q| u32::from(q.max_size)).unwrap_or(0),
                0x44 => self.with_queue(|q| u32::from(q.ready)).unwrap_or(0),
                0x60 => self.interrupt_status.load(Ordering::SeqCst),
                0x70 => self.device_status,
                0xfc => self.config_generation.0,
                _ => {
                    return;
                }
            };
            data.copy_from_slice(v.to_le_bytes().as_slice());
        } else if offset >= CONFIG_SPACE_OFFSET && offset <= 0xfff {
            self.device
                .lock()
                .unwrap()
                .read_config((offset - CONFIG_SPACE_OFFSET) as u32, data);
        }
    }

    // pub fn reason(offset: u64, data: &mut [u8]) {
    //     if offset <= 0xff && data.len() == 4 {}
    // }

    pub fn bus_write(&mut self, offset: u64, data: &[u8]) {
        fn set_high32(v: &mut GuestAddress, x: u32) {
            *v = (*v & 0xffff_ffff) | (u64::from(x) << 32)
        }

        fn set_low32(v: &mut GuestAddress, x: u32) {
            *v = (*v & !0xffff_ffff) | u64::from(x)
        }

        if offset <= 0xff && data.len() == std::mem::size_of::<u32>() {
            let mut buf: [u8; 4] = [0; 4];
            buf.copy_from_slice(data);
            let v = u32::from_le_bytes(buf);
            match offset {
                0x14 => self.features_select = v,
                0x20 => {
                    if self.check_device_status(
                        device_status::DRIVER,
                        device_status::FEATURES_OK
                            | device_status::FAILED
                            | device_status::DEVICE_NEEDS_RESET,
                    ) {
                        self.device
                            .lock()
                            .unwrap()
                            .ack_features_by_page(self.acked_features_select, v);
                    }
                }
                0x24 => self.acked_features_select = v,
                0x30 => self.queue_select = v,
                0x38 => self.update_queue_field(|q| q.size = (v & 0xffff) as u16),
                0x44 => self.update_queue_field(|q| q.ready = v == 1),
                0x64 => {
                    if self.check_device_status(device_status::DRIVER_OK, 0) {
                        self.interrupt_status.fetch_and(!v, Ordering::SeqCst);
                    }
                }
                0x70 => self.set_device_status(v),
                0x80 => self.update_queue_field(|q| set_low32(&mut q.desc_table, v)),
                0x84 => self.update_queue_field(|q| set_high32(&mut q.desc_table, v)),
                0x90 => self.update_queue_field(|q| set_low32(&mut q.avail_ring, v)),
                0x94 => self.update_queue_field(|q| set_high32(&mut q.avail_ring, v)),
                0xa0 => self.update_queue_field(|q| set_low32(&mut q.used_ring, v)),
                0xa4 => self.update_queue_field(|q| set_high32(&mut q.used_ring, v)),
                _ => {}
            }
        } else if offset >= CONFIG_SPACE_OFFSET && offset <= 0xfff {
            if self.check_device_status(
                device_status::DRIVER,
                device_status::FAILED | device_status::DEVICE_NEEDS_RESET,
            ) {
                self.device
                    .lock()
                    .unwrap()
                    .write_config((offset - CONFIG_SPACE_OFFSET) as u32, data);
                self.config_generation += 1;
            }
        }
    }

    fn features_for_page(&self) -> u32 {
        let mut features = self
            .device
            .lock()
            .unwrap()
            .avail_features_by_page(self.features_select);
        if self.features_select == 1 {
            features |= 1; // VIRTIO_F_VERSION_1
        }
        features
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use vm_memory::{Address, ByteValued, Bytes, GuestAddress, GuestMemoryMmap};
    use vm_superio::Trigger;
    use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Descriptor {
        addr: u64,
        len: u32,
        flags: u16,
        next: u16,
    }

    unsafe impl ByteValued for Descriptor {}

    #[derive(Clone)]
    struct CountingTrigger {
        calls: Arc<AtomicUsize>,
    }

    impl Trigger for CountingTrigger {
        type E = io::Error;

        fn trigger(&self) -> Result<(), Self::E> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct FakeDevice {
        avail_features: u64,
        acked_features: u64,
        queues: Vec<Queue>,
        events: Vec<EventFd>,
        irq_status: Arc<AtomicU32>,
        trigger: CountingTrigger,
        config: Vec<u8>,
        activated: bool,
        activate_calls: usize,
        reset_calls: usize,
        reset_ok: bool,
    }

    impl FakeDevice {
        fn new(avail_features: u64, queue_count: usize, reset_ok: bool) -> Self {
            Self {
                avail_features,
                acked_features: 0,
                queues: (0..queue_count).map(|_| Queue::new(8)).collect(),
                events: (0..queue_count)
                    .map(|_| EventFd::new(EFD_NONBLOCK).unwrap())
                    .collect(),
                irq_status: Arc::new(AtomicU32::new(0)),
                trigger: CountingTrigger {
                    calls: Arc::new(AtomicUsize::new(0)),
                },
                config: vec![1, 2, 3, 4, 5, 6, 7, 8],
                activated: false,
                activate_calls: 0,
                reset_calls: 0,
                reset_ok,
            }
        }
    }

    impl VirtioDevice for FakeDevice {
        fn avail_features(&self) -> u64 {
            self.avail_features
        }
        fn acked_features(&self) -> u64 {
            self.acked_features
        }
        fn set_acked_features(&mut self, acked_features: u64) {
            self.acked_features = acked_features;
        }
        fn device_type(&self) -> u32 {
            0xfeed
        }
        fn queues(&self) -> &[Queue] {
            self.queues.as_slice()
        }
        fn queues_mut(&mut self) -> &mut [Queue] {
            self.queues.as_mut_slice()
        }
        fn queues_events(&self) -> &[EventFd] {
            self.events.as_slice()
        }
        fn interrupt_trigger(&self) -> &dyn Trigger<E = io::Error> {
            &self.trigger
        }
        fn interrupt_status(&self) -> Arc<AtomicU32> {
            Arc::clone(&self.irq_status)
        }
        fn read_config(&self, offset: u32, data: &mut [u8]) {
            let start = offset as usize;
            let len = self.config.len().saturating_sub(start).min(data.len());
            data[..len].copy_from_slice(&self.config[start..start + len]);
        }
        fn write_config(&mut self, _offset: u32, _data: &[u8]) {}
        fn activate(&mut self, _mem: GuestMemoryMmap) -> io::Result<()> {
            self.activate_calls += 1;
            self.activated = true;
            Ok(())
        }
        fn is_activated(&self) -> bool {
            self.activated
        }
        fn reset(&mut self) -> Option<(EventFd, Vec<EventFd>)> {
            self.reset_calls += 1;
            if !self.reset_ok {
                return None;
            }
            self.activated = false;
            Some((EventFd::new(EFD_NONBLOCK).unwrap(), Vec::new()))
        }
    }

    fn transport_with_device(device: FakeDevice) -> (Arc<Mutex<FakeDevice>>, MMIOTransport) {
        let device = Arc::new(Mutex::new(device));
        let mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x20_000)]).unwrap();
        let transport_device: Arc<Mutex<dyn VirtioDevice>> = device.clone();
        let transport = MMIOTransport::new(transport_device, mem);
        (device, transport)
    }

    fn prepare_valid_queue(mem: &GuestMemoryMmap<()>, queue: &mut Queue) {
        queue.size = 8;
        queue.ready = true;
        queue.desc_table = GuestAddress(0x1000);
        queue.avail_ring = GuestAddress(0x2000);
        queue.used_ring = GuestAddress(0x3000);

        mem.write_obj(
            Descriptor {
                addr: 0x4000,
                len: 8,
                flags: 0,
                next: 0,
            },
            queue.desc_table,
        )
        .unwrap();
        mem.write_obj(0u16, queue.avail_ring).unwrap();
        mem.write_obj(1u16, queue.avail_ring.unchecked_add(2))
            .unwrap();
        mem.write_obj(0u16, queue.avail_ring.unchecked_add(4))
            .unwrap();
    }

    // Basic MMIO registers read back right values.
    #[test]
    fn mmio_reports_basic_registers_and_features() {
        let (_device, mut transport) =
            transport_with_device(FakeDevice::new((1u64 << 40) | 0x12, 1, true));
        let mut data = [0u8; 4];

        transport.bus_read(0x0, &mut data);
        assert_eq!(u32::from_le_bytes(data), 0x7472_6976);

        transport.bus_read(0x4, &mut data);
        assert_eq!(u32::from_le_bytes(data), 2);

        transport.bus_read(0x8, &mut data);
        assert_eq!(u32::from_le_bytes(data), 0xfeed);

        transport.bus_write(0x14, &0u32.to_le_bytes());
        transport.bus_read(0x10, &mut data);
        assert_eq!(u32::from_le_bytes(data), 0x12);

        transport.bus_write(0x14, &1u32.to_le_bytes());
        transport.bus_read(0x10, &mut data);
        assert_eq!(u32::from_le_bytes(data), 0x101);
    }

    // Unsupported feature bits are masked out per page.
    #[test]
    fn mmio_ack_features_masks_unsupported_bits() {
        let (device, mut transport) =
            transport_with_device(FakeDevice::new((1u64 << 40) | 0x12, 1, true));

        transport.bus_write(0x70, &1u32.to_le_bytes());
        transport.bus_write(0x70, &3u32.to_le_bytes());
        transport.bus_write(0x24, &1u32.to_le_bytes());
        transport.bus_write(0x20, &u32::MAX.to_le_bytes());

        {
            let guard = device.lock().unwrap();
            assert_eq!(guard.acked_features, 1u64 << 40);
        }

        transport.bus_write(0x24, &0u32.to_le_bytes());
        transport.bus_write(0x20, &u32::MAX.to_le_bytes());

        let guard = device.lock().unwrap();
        assert_eq!(guard.acked_features, (1u64 << 40) | 0x12);
    }

    // Queue writes wait for FEATURES_OK.
    #[test]
    fn mmio_queue_writes_apply_only_after_features_ok() {
        let (device, mut transport) = transport_with_device(FakeDevice::new(0, 1, true));
        let mut data = [0u8; 4];

        transport.bus_write(0x30, &0u32.to_le_bytes());
        transport.bus_write(0x38, &8u32.to_le_bytes());
        assert_eq!(device.lock().unwrap().queues[0].size, 0);

        transport.bus_write(0x70, &1u32.to_le_bytes());
        transport.bus_write(0x70, &3u32.to_le_bytes());
        transport.bus_write(0x70, &11u32.to_le_bytes());
        transport.bus_write(0x38, &8u32.to_le_bytes());
        assert_eq!(device.lock().unwrap().queues[0].size, 8);

        transport.bus_read(0x34, &mut data);
        assert_eq!(u32::from_le_bytes(data), 8);
    }

    // Valid queues activate device.
    #[test]
    fn mmio_driv_ok_activates_valid_queues() {
        let (device, _transport) = transport_with_device(FakeDevice::new(0, 1, true));
        let mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x20_000)]).unwrap();
        {
            let mut guard = device.lock().unwrap();
            prepare_valid_queue(&mem, &mut guard.queues[0]);
        }
        let transport_device: Arc<Mutex<dyn VirtioDevice>> = device.clone();
        let mut transport = MMIOTransport::new(transport_device, mem.clone());

        for value in [1u32, 3] {
            transport.bus_write(0x70, &value.to_le_bytes());
        }
        transport.bus_write(0x20, &0u32.to_le_bytes());
        transport.bus_write(0x24, &0u32.to_le_bytes());
        transport.bus_write(0x70, &11u32.to_le_bytes());
        transport.bus_write(0x30, &0u32.to_le_bytes());
        transport.bus_write(0x38, &8u32.to_le_bytes());
        transport.bus_write(0x44, &1u32.to_le_bytes());
        transport.bus_write(0x80, &0x1000u32.to_le_bytes());
        transport.bus_write(0x90, &0x2000u32.to_le_bytes());
        transport.bus_write(0xa0, &0x3000u32.to_le_bytes());
        transport.bus_write(0x84, &0u32.to_le_bytes());
        transport.bus_write(0x94, &0u32.to_le_bytes());
        transport.bus_write(0xa4, &0u32.to_le_bytes());
        transport.bus_write(0x70, &15u32.to_le_bytes());

        let guard = device.lock().unwrap();
        assert!(guard.activated);
        assert_eq!(guard.activate_calls, 1);
    }

    // Status zero resets device state.
    #[test]
    fn mmio_reset_clears_state_on_zero_status() {
        let (device, _transport) = transport_with_device(FakeDevice::new(0, 1, true));
        let mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x20_000)]).unwrap();
        {
            let mut guard = device.lock().unwrap();
            prepare_valid_queue(&mem, &mut guard.queues[0]);
        }
        let transport_device: Arc<Mutex<dyn VirtioDevice>> = device.clone();
        let mut transport = MMIOTransport::new(transport_device, mem.clone());

        for value in [1u32, 3, 11, 15] {
            transport.bus_write(0x70, &value.to_le_bytes());
        }

        transport.bus_write(0x70, &0u32.to_le_bytes());

        let mut data = [0u8; 4];
        transport.bus_read(0x70, &mut data);
        assert_eq!(u32::from_le_bytes(data), 0);

        let guard = device.lock().unwrap();
        assert_eq!(guard.reset_calls, 1);
        assert!(!guard.activated);
        assert_eq!(guard.queues[0].size, 0);
        assert!(!guard.queues[0].ready);
    }

    // Config space read routes to device.
    #[test]
    fn mmio_config_reads_forward_to_device() {
        let (_device, mut transport) = transport_with_device(FakeDevice::new(0, 1, true));
        let mut data = [0u8; 4];

        transport.bus_read(0x100, &mut data);
        assert_eq!(data, [1, 2, 3, 4]);
    }
}
