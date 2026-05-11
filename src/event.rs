use std::{cell::RefCell, io, ops::Deref, os::fd::AsRawFd, rc::Rc};

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

pub struct EventLoop<T> {
    epoll: Rc<Epoll>,
    events: Vec<EpollEvent>,
    callbacks: Rc<RefCell<Slab<T>>>,
}

impl<T> EventLoop<T> {
    pub fn new() -> io::Result<Self> {
        Self::with_capacity(DEFAULT_EPOLL_EVENT_CAP)
    }

    pub fn with_capacity(size: usize) -> io::Result<Self> {
        let epoll = Rc::new(Epoll::new()?);
        Ok(Self {
            epoll,
            events: vec![EpollEvent::new(EventSet::empty(), 0); size],
            callbacks: Rc::new(RefCell::new(Slab::new())),
        })
    }

    pub fn register(&mut self, fd: &impl AsRawFd, event: EventSet, callback: T) -> io::Result<()> {
        let fd = fd.as_raw_fd();
        let user_data = self.callbacks.borrow_mut().insert(callback);
        if let Err(e) = self.add_epoll_entry(fd, event, user_data) {
            self.callbacks.borrow_mut().remove(user_data);
            return Err(e);
        }
        Ok(())
    }

    fn add_epoll_entry(
        &self,
        fd: libc::c_int,
        event: EventSet,
        user_data: usize,
    ) -> io::Result<()> {
        self.epoll.ctl(
            ControlOperation::Add,
            fd,
            EpollEvent::new(event, user_data as u64),
        )
    }
}

pub trait EventHandlerMut {
    fn handle_event_mut(&mut self, events: EventSet);
}

pub trait EventHandler {
    fn handle_event(&self, events: EventSet);
}

impl<T> EventLoop<T>
where
    T: EventHandlerMut,
{
    pub fn tick(&mut self, timeout: i32) -> io::Result<usize> {
        let num_events = self.epoll.wait(timeout, &mut self.events[..])?;
        let mut callbacks = self.callbacks.borrow_mut();
        for event in self.events.iter().take(num_events) {
            let index = event.data() as usize;
            callbacks[index].handle_event_mut(event.event_set());
        }
        Ok(num_events)
    }
}

#[derive(Debug)]
pub struct PreRegister<T> {
    epoll: Rc<Epoll>,
    callbacks: Rc<RefCell<Slab<T>>>,
    user_data: usize,
}

impl<T> EventLoop<T> {
    pub fn pre_register(&mut self, callback: T) -> PreRegister<T> {
        let user_data = self.callbacks.borrow_mut().insert(callback);
        PreRegister {
            epoll: self.epoll.clone(),
            callbacks: self.callbacks.clone(),
            user_data,
        }
    }
}

impl<T> PreRegister<T> {
    pub fn associate(&self, fd: &impl AsRawFd, event: EventSet) -> io::Result<()> {
        let fd = fd.as_raw_fd();
        if let Err(e) = self.epoll.ctl(
            ControlOperation::Add,
            fd,
            EpollEvent::new(event, self.user_data as u64),
        ) {
            self.callbacks.borrow_mut().remove(self.user_data);
            return Err(e);
        }
        Ok(())
    }
}
