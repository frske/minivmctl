use minivmctl::event::EventWrapper;
use vm_superio::Trigger;

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
