use std::{
    io,
    sync::{Arc, atomic::Ordering},
};

use virtio_device::VirtioConfig;
use virtio_queue::{Queue, QueueT};
use vm_superio::Trigger;
use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};

const IRQ_BIT_VRING: u8 = 0x1;

pub(crate) const fn feature_bit(bit: u32) -> u64 {
    1u64 << bit
}

pub(crate) struct IrqTrigger {
    irq_evt: EventFd,
    irq_status: Arc<std::sync::atomic::AtomicU8>,
}

impl IrqTrigger {
    pub(crate) fn new(irq_status: Arc<std::sync::atomic::AtomicU8>) -> io::Result<Self> {
        Ok(Self {
            irq_evt: EventFd::new(EFD_NONBLOCK)?,
            irq_status,
        })
    }

    pub(crate) fn event(&self) -> &EventFd {
        &self.irq_evt
    }

    pub(crate) fn trigger_vring(&self) -> io::Result<()> {
        self.irq_status.fetch_or(IRQ_BIT_VRING, Ordering::SeqCst);
        self.irq_evt.write(1)
    }
}

impl Trigger for IrqTrigger {
    type E = io::Error;

    fn trigger(&self) -> Result<(), Self::E> {
        self.trigger_vring()
    }
}

//Not functional, Dani would be sad
pub(crate) fn reset_config(cfg: &mut VirtioConfig<Queue>) {
    cfg.driver_features = 0;
    cfg.device_features_select = 0;
    cfg.driver_features_select = 0;
    cfg.device_status = 0;
    cfg.queue_select = 0;
    cfg.interrupt_status.store(0, Ordering::SeqCst);
    cfg.device_activated = false;

    for queue in &mut cfg.queues {
        queue.reset();
    }
}
