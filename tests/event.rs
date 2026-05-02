use std::sync::{Arc, Mutex};

use scratch_new::event::{EventHandlerMut, EventLoop, EventWrapper};
use vm_superio::Trigger;
use vmm_sys_util::epoll::EventSet;

struct Recorder {
    seen: Arc<Mutex<Vec<EventSet>>>,
}

impl EventHandlerMut for Recorder {
    fn handle_event_mut(&mut self, events: EventSet) {
        self.seen.lock().unwrap().push(events);
    }
}

// Trigger makes eventfd readable.
#[test]
fn event_wrapper_triggers_readable_eventfd() {
    let evt = EventWrapper::new();

    evt.trigger().unwrap();

    assert_eq!(evt.read().unwrap(), 1);
}

// Clone keeps same wakeup.
#[test]
fn event_wrapper_clone_is_independent_handle() {
    let evt = EventWrapper::new();
    let clone = evt.try_clone().unwrap();

    evt.trigger().unwrap();

    assert_eq!(clone.read().unwrap(), 1);
}

// Registered callback fires on tick.
#[test]
fn event_loop_dispatches_registered_callback() {
    let mut loop_ = EventLoop::new().unwrap();
    let evt = EventWrapper::new();
    let seen = Arc::new(Mutex::new(Vec::new()));

    loop_
        .register(&evt.0, EventSet::IN, Recorder { seen: seen.clone() })
        .unwrap();

    evt.trigger().unwrap();
    loop_.tick(0).unwrap();

    assert!(!seen.lock().unwrap().is_empty());
}

// Late bind still works.
#[test]
fn pre_register_associates_fd_later() {
    let mut loop_ = EventLoop::new().unwrap();
    let evt = EventWrapper::new();
    let seen = Arc::new(Mutex::new(Vec::new()));

    let pre = loop_.pre_register(Recorder { seen: seen.clone() });
    pre.associate(&evt.0, EventSet::IN).unwrap();

    evt.trigger().unwrap();
    loop_.tick(0).unwrap();

    assert!(!seen.lock().unwrap().is_empty());
}
