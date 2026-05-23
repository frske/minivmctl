use std::ops::Deref;

use vm_superio::Trigger;
use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};

pub struct EventWrapper(pub EventFd);

impl EventWrapper {
    pub fn new() -> Self {
        Self(EventFd::new(EFD_NONBLOCK).unwrap())
    }

    pub fn try_clone(&self) -> std::io::Result<Self> {
        self.0.try_clone().map(Self)
    }
}

impl Default for EventWrapper {
    fn default() -> Self {
        Self::new()
    }
}

impl Deref for EventWrapper {
    type Target = EventFd;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Trigger for EventWrapper {
    type E = std::io::Error;

    fn trigger(&self) -> Result<(), Self::E> {
        self.write(1)
    }
}
