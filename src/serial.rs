use vm_superio::serial::SerialEvents;

pub struct DummySerialEvent;

impl SerialEvents for DummySerialEvent {
    fn buffer_read(&self) {}
    fn out_byte(&self) {}
    fn tx_lost_byte(&self) {}
    fn in_buffer_empty(&self) {}
}
