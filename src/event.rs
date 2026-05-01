use std::{io, mem::zeroed, ops::Deref, os::fd::AsRawFd, rc::Rc};

use slab::Slab;
use vm_superio::Trigger;
use vmm_sys_util::{
    epoll::{ControlOperation, Epoll, EpollEvent, EventSet},
    eventfd::{EFD_NONBLOCK, EventFd},
};

pub struct EventWrapper(pub EventFd);
const DEFAULT_EPOLL_EVENT_CAP: usize = 256;

impl EventWrapper {
    pub fn new() -> Self {
        Self(EventFd::new(EFD_NONBLOCK).unwrap())
    }

    pub fn try_clone(&self) -> std::io::Result<Self> {
        self.0.try_clone().map(Self)
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

pub struct EventLoop<T> {
    epoll: Rc<Epoll>,
    events: Vec<EpollEvent>,
    callbacks: Slab<T>,
}

impl<T> EventLoop<T> {
    pub fn new() -> io::Result<Self> {
        Self::with_capacity(DEFAULT_EPOLL_EVENT_CAP)
    }

    pub fn with_capacity(size: usize) -> io::Result<Self> {
        let epoll = Rc::new(Epoll::new()?);
        Ok(Self {
            epoll,
            events: vec![unsafe { zeroed() }; size],
            callbacks: Slab::new(),
        })
    }

    pub fn register(&mut self, fd: &impl AsRawFd, event: EventSet, callback: T) -> io::Result<()> {
        let fd = fd.as_raw_fd();
        let user_data = self.callbacks.insert(callback);
        self.epoll
            .ctl(
                ControlOperation::Add,
                fd,
                EpollEvent::new(event, user_data as u64),
            )
            .map_err(|e| {
                let _ = self.callbacks.remove(user_data);
                e
            })?;
        Ok(())
    }
}

pub trait EventHanderMut {
    fn handle_event_mut(&mut self, events: EventSet);
}

pub trait EventHandler {
    fn handle_event(&self, events: EventSet);
}

impl<T> EventLoop<T>
where
    T: EventHanderMut,
{
    pub fn tick(&mut self, timeout: i32) -> io::Result<usize> {
        let n = self.epoll.wait(timeout, &mut self.events[..])?;
        for event in self.events.iter().take(n) {
            self.callbacks[event.data() as usize].handle_event_mut(event.event_set());
        }
        Ok(n)
    }
}

#[derive(Debug)]
pub struct PreRegister {
    epoll: Rc<Epoll>,
    user_data: usize,
}

impl<T> EventLoop<T> {
    pub fn pre_register(&mut self, callback: T) -> PreRegister {
        let user_data = self.callbacks.insert(callback);
        PreRegister {
            epoll: self.epoll.clone(),
            user_data,
        }
    }
}

impl PreRegister {
    pub fn associate(&self, fd: &impl AsRawFd, event: EventSet) -> io::Result<()> {
        let fd = fd.as_raw_fd();
        self.epoll.ctl(
            ControlOperation::Add,
            fd,
            EpollEvent::new(event, self.user_data as u64)
        )?;
        Ok(())
    }
}
