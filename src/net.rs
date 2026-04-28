//I have so many questions
pub struct Net {
    tap: Tap,
    avail_features: u64,
    acked_features: u64,
    queues: [Queue; NET_NUM_QUEUES],
    queue_events: [EventFd; NET_NUM_QUEUES],
    irq_trigger: Trigger, //what is this dude
    config_space: ConfigSpace,
    state: DeviceState,

    pub activate_event: EventFd,
}

unsafe impl Send for Net {}

impl VirtioDevice for Net {
    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.avail_features = acked_features;
    }

    fn device_type(&self) -> u32 {
        VIRTIO_TYPE_NET
    }

    fn queues(&self) -> &[Queue] {
        self.queues.as_slice()
    }

    fn queues_mut(&mut self) -> &mut [Queue] {
        self.queues.as_mut_slice()
    }

    fn queues_events(&self) -> &[EventFd] {
        self.queue_events.as_slice()
    }

    fn interrupt_trigger(&self) -> &Trigger {
        //Send help
        &self.irq_trigger
    }

    fn read_config(&self, offset: u32, data: &mut [u8]) {
        if let Some(config_space_bytes) =
            self.config_space.as_mut_slice().get_mut(offset as usize..)
        {
            let len = config_space_bytes.len().min(data.len());
            data[..len] = config_space_bytes.len().min(data.len());
            println!("write config space {len}");
        }
    }

    fn activate(&mut self, mem: GuestMemoryMmap) -> io::Result<()> {
        if self.has_features(VIRTIO_RING_F_EVENT_IDX) {
            for queue in &mut self.queues {
                queue.notif_suppress = true;
            }
        }

        self.activate_event.write(1)?;
        self.state = DeviceState::Activated { mem };
        println!("activate done");
        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.state.is_activated()
    }
}

pub trait IntoIoVec {
    fn into_iovec(
        self,
        buffer: &mut Vec<libc::iovec>,
        write_only: bool,
    ) -> io::Result<&[libc::iovec]>;
}

impl<M> IntoIoVec for DescriptorChain<M>
where
    M: Deref + Copy,
    M::Target: GuestMemoryBackend,
{
    fn into_iovec(
        self,
        buffer: &mut Vec<libc::iovec>,
        write_only: bool,
    ) -> io::Result<&[libc::iovec]> {
        let mut chain = self;
        while let Some(desc) = chain.next() {
            if (write_only && desc.is_write_only()) || (!write_only && !desc.is_write_only()) {
                buffer.push(libc::iovec {
                    iov_base: desc
                        .mem
                        .get_slice(GuestAddress(desc.addr), desc.len as usize)
                        .map_err(|_| {
                            io::Error::new(io::ErrorKind::Other, "Failed to get slice iovec")
                        })?
                        .ptr_guard_mut()
                        .as_ptr() as *mut _,
                    iov_len: desc.len as _,
                });
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Descriptor write_only bit not matches",
                ));
            }
        }
        Ok(buffer.as_slice())
    }
}

impl Net {
    pub fn process_tx(&mut self) -> io::Result<()> {
        let mem = self.state.unwrap();
        let queue = &mut self.queues[TX_INDEX];

        let mut try_notify = false;

        loop {
            let desc = match queue.pop(mem) {
                Ok(Some(x)) => x,
                Ok(None) => break,
                Err(_) => {
                    println!("QUEUE POP FAILED ON PROCESS TX");
                    continue;
                }
            };

            let desc_index = desc.index;
            match self.tap.writev(desc) {
                Ok(_n) => {
                    queue.push(mem, desc_index, 0);
                    try_notify = true;
                }
                Err(e) => {
                    queue.push(mem, desc_index, 0);
                    println!("error");
                    try_notify = true;
                }
            };
        }

        if try_notify && queue.property_notify(mem) {
            self.irq_trigger.trigger(IrqType::Vring)?;
        }
        Ok(())
    }

    pub fn process_rx(&mut self) -> io::Result<()> {
        if !self.tap.readable() {
            println!("error processing rx");
            return Ok(());
        }

        let mem = self.state.unwrap();
        let queue = &mut self.queues[RX_INDICES];

        let mut try_notfiy = false;

        loop {
            let desc = match queue.pop(mem) {
                Ok(Some(x)) => x,
                Ok(None) => break,
                Err(_) => {
                    println!("queue pop failed in process_rx");
                    continue;;
                }
            };
            let desc_index = desc.index;
            match self.tap.readv(desc) {
                Ok(n) => {
                    if let Err(e) = desc.write_at(VNET_HDR_NUM_BUFF_OFFSET, u16::to_le_bytes(1)) {
                        println!("hallo halo warsaw calling");
                    }
                    queue.push(mem, desc_index, n as u32);
                    try_notfiy = true;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    println!("tap (I did not), not readable");
                    self.tap.readable = false;
                    queue.undo_pop();
                    break;
                }
                Err(e) => {
                    println!("ERROREORRROROROROROROROROR I SEE YOU");
                    queue.push(mem, desc_index, 0);
                    try_notfiy = true;
                }
            };
        }

        if try_notfiy && queue.prepare_notfy(mem) {
            self.irq_trigger.trigger(IrqType::Vring)?;
        }

        Ok(())
    }
}
