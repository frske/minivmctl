use std::{
    borrow::{Borrow, BorrowMut},
    fs::File,
    io,
    sync::Arc,
};

use virtio_bindings::bindings::{
    virtio_blk::VIRTIO_BLK_F_FLUSH, virtio_config::VIRTIO_F_VERSION_1,
};
use virtio_bindings::virtio_ids::VIRTIO_ID_BLOCK;
use virtio_blk::{request::Request, stdio_executor::StdIoBackend};
use virtio_device::{VirtioConfig, VirtioDeviceActions, VirtioDeviceType, VirtioMmioDevice};
use virtio_queue::{Queue, QueueT};
use vm_memory::GuestMemoryMmap;
use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};

use crate::virtio_common::{IrqTrigger, feature_bit, reset_config};

const BLK_QUEUE_SIZE: u16 = 256;

pub struct Blk {
    cfg: VirtioConfig<Queue>,
    disk_image: Option<File>,
    executor: Option<StdIoBackend<File>>,
    queue_events: [EventFd; 1],
    irq_trigger: IrqTrigger,
    mem: GuestMemoryMmap,
}

impl Blk {
    pub fn new(disk_image: File, mem: GuestMemoryMmap) -> io::Result<Self> {
        let disk_size = disk_image.metadata()?.len();
        let capacity_sectors = disk_size / virtio_blk::defs::SECTOR_SIZE;
        let mut config_space = vec![0; 8];
        config_space[..8].copy_from_slice(&capacity_sectors.to_le_bytes());

        let queues = vec![Queue::new(BLK_QUEUE_SIZE).map_err(io::Error::other)?];
        let cfg = VirtioConfig::new(
            feature_bit(VIRTIO_F_VERSION_1) | feature_bit(VIRTIO_BLK_F_FLUSH),
            queues,
            config_space,
        );
        let irq_trigger = IrqTrigger::new(Arc::clone(&cfg.interrupt_status))?;

        Ok(Self {
            cfg,
            disk_image: Some(disk_image),
            executor: None,
            queue_events: [EventFd::new(EFD_NONBLOCK)?],
            irq_trigger,
            mem,
        })
    }

    pub fn interrupt_event(&self) -> &EventFd {
        self.irq_trigger.event()
    }

    pub fn queues_events(&self) -> &[EventFd] {
        self.queue_events.as_slice()
    }

    pub fn process_queue(&mut self) -> io::Result<()> {
        let mem = self.mem.clone();
        let executor = self.executor.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "blk device not activated")
        })?;
        let queue = &mut self.cfg.queues[0];
        let mut used_any = false;

        while let Some(mut chain) = queue.pop_descriptor_chain(&mem) {
            let head_index = chain.head_index();
            let used_len = match Request::parse(&mut chain) {
                Ok(request) => executor.process_request(&mem, &request).unwrap_or_default(),
                Err(_) => 0,
            };
            if queue.add_used(&mem, head_index, used_len).is_ok() {
                used_any = true;
            }
        }

        if used_any && queue.needs_notification(&mem).unwrap_or(true) {
            self.irq_trigger.trigger_vring()?;
        }

        Ok(())
    }
}

impl Borrow<VirtioConfig<Queue>> for Blk {
    fn borrow(&self) -> &VirtioConfig<Queue> {
        &self.cfg
    }
}

impl BorrowMut<VirtioConfig<Queue>> for Blk {
    fn borrow_mut(&mut self) -> &mut VirtioConfig<Queue> {
        &mut self.cfg
    }
}

impl VirtioDeviceType for Blk {
    fn device_type(&self) -> u32 {
        VIRTIO_ID_BLOCK
    }
}

impl VirtioDeviceActions for Blk {
    type E = io::Error;

    fn activate(&mut self) -> Result<(), Self::E> {
        if self.executor.is_none() {
            let disk_image = self.disk_image.take().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "missing block backend")
            })?;
            self.executor = Some(
                StdIoBackend::new(disk_image, self.cfg.driver_features)
                    .map_err(|e| io::Error::other(e.to_string()))?,
            );
        }
        self.cfg.device_activated = true;
        Ok(())
    }

    fn reset(&mut self) -> Result<(), Self::E> {
        if let Some(executor) = self.executor.take() {
            self.disk_image = Some(executor.into_inner());
        }
        reset_config(&mut self.cfg);
        Ok(())
    }
}

impl VirtioMmioDevice for Blk {}
