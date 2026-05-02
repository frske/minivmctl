use std::{
    io,
    num::Wrapping,
    ops::Deref,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering, fence},
    },
};

use vm_memory::{Address, ByteValued, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};
use vm_superio::Trigger;
use vmm_sys_util::eventfd::EventFd;

#[derive(Clone)]
pub struct DescriptorChain<M> {
    pub(crate) mem: M,
    desc_table: GuestAddress,
    queue_size: u16,
    pub(crate) index: u16,

    pub(crate) addr: u64,  //memroy address in Guest
    pub(crate) len: u32,   //length of buffer
    pub(crate) flags: u16, //readable, writable, has next
    pub(crate) next: u16,  //next descriptor if index chained
}

impl<M> DescriptorChain<M>
where
    M: Deref,
    M::Target: GuestMemoryBackend,
{
    pub fn new(mem: M, desc_table: GuestAddress, queue_size: u16, index: u16) -> Result<Self, ()> {
        if index >= queue_size {
            eprintln!("invalid index {index}, queue size: {queue_size}");
            return Err(());
        }
        Ok(Self::new_unchecked(mem, desc_table, queue_size, index))
    }

    #[inline(always)]
    pub fn new_unchecked(mem: M, desc_table: GuestAddress, queue_size: u16, index: u16) -> Self {
        #[repr(C)]
        #[derive(Default, Clone, Copy)]
        struct Descriptor {
            addr: u64,
            len: u32,
            flags: u16,
            next: u16,
        }
        unsafe impl ByteValued for Descriptor {}

        let addr = desc_table.unchecked_add((index as u64) * 16);
        let desc = mem.read_obj::<Descriptor>(addr).unwrap();

        Self {
            mem,
            desc_table,
            queue_size,
            index,
            addr: desc.addr,
            len: desc.len,
            flags: desc.flags,
            next: desc.next,
        }
    }

    pub fn next(&self) -> Option<Self>
    where
        M: Clone,
    {
        const VIRTQ_DESC_F_NEXT: u16 = 0x1;
        if self.flags & VIRTQ_DESC_F_NEXT != 0 {
            Self::new(
                self.mem.clone(),
                self.desc_table,
                self.queue_size,
                self.next,
            )
            .ok()
        } else {
            None
        }
    }

    #[inline]
    pub const fn is_write_only(&self) -> bool {
        const VIRTQ_DESC_F_WRITE: u16 = 0x2;
        self.flags & VIRTQ_DESC_F_WRITE != 0
    }

    #[inline]
    pub fn write_at(self, mut offset: usize, mut data: &[u8]) -> io::Result<()>
    where
        M: Clone,
    {
        if data.is_empty() {
            return Ok(());
        }
        let mut next_desc = Some(self);
        while let Some(desc) = next_desc {
            let skip = offset.min(desc.len as usize);
            offset -= skip;
            if offset == 0 {
                let to_copy = data.len().min(desc.len as usize - skip);
                desc.mem
                    .write_slice(&data[..to_copy], GuestAddress(desc.addr + skip as u64))
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::Other, "Failed to write slice in mem")
                    })?;
                data = &data[to_copy..];
                if data.is_empty() {
                    return Ok(());
                }
            }
            next_desc = desc.next();
        }
        Err(io::Error::new(
            io::ErrorKind::Other,
            "Failed to write slice",
        ))
    }
}

pub struct Queue {
    pub(crate) desc_table: GuestAddress,
    pub(crate) avail_ring: GuestAddress,
    pub(crate) used_ring: GuestAddress,

    pub(crate) size: u16,
    pub(crate) ready: bool,

    pub(crate) next_avail: Wrapping<u16>,
    pub(crate) next_used: Wrapping<u16>,
    pub(crate) num_added: Wrapping<u16>,

    pub(crate) max_size: u16,
    pub(crate) notif_suppress: bool,
}

impl Queue {
    pub const fn new(max_size: u16) -> Self {
        Self {
            max_size,
            size: 0,
            ready: false,
            desc_table: GuestAddress(0),
            avail_ring: GuestAddress(0),
            used_ring: GuestAddress(0),
            next_avail: Wrapping(0),
            next_used: Wrapping(0),
            notif_suppress: false,
            num_added: Wrapping(0),
        }
    }

    pub fn pop<M: Deref<Target = impl GuestMemoryBackend>>(
        &mut self,
        mem: M,
    ) -> Result<Option<DescriptorChain<M>>, ()> {
        macro_rules! try_pop {
            () => {
                if self.len(mem.deref()) != 0 {
                    return self.pop_unchecked(mem).map(Option::Some);
                }
            };
        }
        fence(Ordering::Acquire);
        try_pop!();
        if self.notif_suppress {
            let avail_event_addr = self.used_ring.unchecked_add(((self.size as u64) << 3) + 4);
            mem.write_obj(self.next_avail.0, avail_event_addr).unwrap();
            fence(Ordering::AcqRel);
            try_pop!();
        }
        Ok(None)
    }

    pub fn push<M: GuestMemoryBackend>(&mut self, mem: &M, desc_index: u16, len: u32) {
        #[repr(C)]
        #[derive(Default, Clone, Copy)]
        struct UsedElem {
            id: u32,
            len: u32,
        }
        unsafe impl ByteValued for UsedElem {}
        let addr = self
            .used_ring
            .unchecked_add((((self.next_used.0 % self.size) as u64) << 3) + 4);
        mem.write_obj(
            UsedElem {
                id: desc_index as u32,
                len,
            },
            addr,
        )
        .unwrap();
        self.num_added += 1;
        self.next_used += 1;
        fence(Ordering::Release);
        mem.write_obj(self.next_used.0, self.used_ring.unchecked_add(2))
            .unwrap();
    }

    pub fn undo_pop(&mut self) {
        self.next_avail -= Wrapping(1);
    }

    pub fn prepare_notify<M: GuestMemoryBackend>(&mut self, mem: &M) -> bool {
        if !self.notif_suppress {
            return mem.read_obj::<u16>(self.avail_ring).unwrap() == 0;
        }
        fence(Ordering::AcqRel);
        let used_event_addr = self.avail_ring.unchecked_add(((self.size as u64) << 1) + 4);
        let used_event = Wrapping(mem.read_obj::<u16>(used_event_addr).unwrap());
        let num_added = self.num_added;
        self.num_added = Wrapping(0);
        self.next_used - used_event - Wrapping(1) < num_added
    }

    pub fn is_valid<M: GuestMemoryBackend>(&self, mem: &M) -> bool {
        if !self.ready {
            return false;
        }
        if self.size > self.max_size {
            return false;
        }
        if self.size == 0 || self.size & (self.size - 1) != 0 || self.size > 32768 {
            return false;
        }
        if self.desc_table.raw_value() & 0xf != 0 {
            return false;
        }
        if self.avail_ring.raw_value() & 0x3 != 0 {
            return false;
        }
        if self.used_ring.raw_value() & 0x1 != 0 {
            return false;
        }
        if mem
            .get_slice(self.desc_table, (self.size as usize) << 4)
            .is_err()
        {
            return false;
        }
        if mem
            .get_slice(self.avail_ring, ((self.size as usize) * 2) + 6)
            .is_err()
        {
            return false;
        }
        if mem
            .get_slice(self.used_ring, ((self.size as usize) * 8) + 6)
            .is_err()
        {
            return false;
        }
        true
    }

    fn pop_unchecked<M: Deref<Target = impl GuestMemoryBackend>>(
        &mut self,
        mem: M,
    ) -> Result<DescriptorChain<M>, ()> {
        let addr = self
            .avail_ring
            .unchecked_add((((self.next_avail.0 % self.size) as u64) * 2) + 4);
        let desc_index = mem.read_obj::<u16>(addr).unwrap();
        let desc_chain = DescriptorChain::new(mem, self.desc_table, self.size, desc_index);
        if desc_chain.is_ok() {
            self.next_avail += 1;
        } else {
            eprintln!("pop failed because invalid desc index");
        }
        desc_chain
    }

    #[inline(always)]
    fn avail_idx<M: GuestMemoryBackend>(&self, mem: &M) -> Wrapping<u16> {
        Wrapping(
            mem.read_obj::<u16>(self.avail_ring.unchecked_add(2))
                .unwrap(),
        )
    }

    #[inline(always)]
    fn len<M: GuestMemoryBackend>(&self, mem: &M) -> u16 {
        (self.avail_idx(mem) - self.next_avail).0
    }
}

pub trait VirtioDevice: Send {
    fn avail_features(&self) -> u64;
    fn acked_features(&self) -> u64;
    fn set_acked_features(&mut self, acked_features: u64);
    fn has_features(&self, feature: u64) -> bool {
        (self.acked_features() & feature) != 0
    }
    fn device_type(&self) -> u32;
    fn queues(&self) -> &[Queue];
    fn queues_mut(&mut self) -> &mut [Queue];
    fn queues_events(&self) -> &[EventFd];
    fn interrupt_trigger(&self) -> &dyn Trigger<E = std::io::Error>;
    fn interrupt_status(&self) -> Arc<AtomicU32>;
    fn avail_features_by_page(&self, page: u32) -> u32 {
        match page {
            0 => (self.avail_features() & 0xffff_ffff) as u32,
            1 => (self.avail_features() >> 32) as u32,
            _ => 0u32,
        }
    }
    fn ack_features_by_page(&mut self, page: u32, value: u32) {
        let mut v = match page {
            0 => u64::from(value),
            1 => u64::from(value) << 32,
            _ => 0u64,
        };
        let avail_features = self.avail_features();
        let unrequested_features = v & !avail_features;
        if unrequested_features != 0 {
            v &= !unrequested_features;
        }
        self.set_acked_features(self.acked_features() | v);
    }
    fn read_config(&self, offset: u32, data: &mut [u8]);
    fn write_config(&mut self, offset: u32, data: &[u8]);
    fn activate(&mut self, mem: GuestMemoryMmap) -> io::Result<()>;
    fn is_activated(&self) -> bool;
    fn reset(&mut self) -> Option<(EventFd, Vec<EventFd>)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Descriptor {
        addr: u64,
        len: u32,
        flags: u16,
        next: u16,
    }

    unsafe impl ByteValued for Descriptor {}

    fn guest_memory(size: usize) -> GuestMemoryMmap<()> {
        GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), size)]).unwrap()
    }

    fn write_descriptor(
        mem: &GuestMemoryMmap<()>,
        table: GuestAddress,
        index: u16,
        desc: Descriptor,
    ) {
        mem.write_obj(desc, table.unchecked_add(u64::from(index) * 16))
            .unwrap();
    }

    fn write_u16(mem: &GuestMemoryMmap<()>, addr: GuestAddress, value: u16) {
        mem.write_obj(value, addr).unwrap();
    }

    fn valid_queue() -> (Queue, GuestMemoryMmap<()>) {
        let mem = guest_memory(0x20_000);
        let mut queue = Queue::new(8);
        queue.size = 8;
        queue.ready = true;
        queue.desc_table = GuestAddress(0x1000);
        queue.avail_ring = GuestAddress(0x2000);
        queue.used_ring = GuestAddress(0x3000);
        (queue, mem)
    }

    // Bad descriptor index rejected.
    #[test]
    fn descriptor_chain_rejects_invalid_index() {
        let mem = guest_memory(0x10_000);

        assert!(DescriptorChain::new(&mem, GuestAddress(0x1000), 2, 2).is_err());
    }

    // Chain follows NEXT bit.
    #[test]
    fn descriptor_chain_follows_next_flag() {
        let mem = guest_memory(0x10_000);
        let table = GuestAddress(0x1000);

        write_descriptor(
            &mem,
            table,
            0,
            Descriptor {
                addr: 0x4000,
                len: 4,
                flags: 1,
                next: 1,
            },
        );
        write_descriptor(
            &mem,
            table,
            1,
            Descriptor {
                addr: 0x5000,
                len: 4,
                flags: 0,
                next: 0,
            },
        );

        let chain = DescriptorChain::new(&mem, table, 2, 0).unwrap();
        let next = chain.next().expect("next descriptor");

        assert_eq!(next.index, 1);
        assert_eq!(next.addr, 0x5000);
    }

    // No NEXT bit ends chain.
    #[test]
    fn descriptor_chain_without_next_flag_returns_none() {
        let mem = guest_memory(0x10_000);
        let table = GuestAddress(0x1000);

        write_descriptor(
            &mem,
            table,
            0,
            Descriptor {
                addr: 0x4000,
                len: 4,
                flags: 0,
                next: 1,
            },
        );

        let chain = DescriptorChain::new(&mem, table, 2, 0).unwrap();

        assert!(chain.next().is_none());
    }

    // Writes can span descriptors.
    #[test]
    fn descriptor_chain_write_at_spans_descriptors() {
        let mem = guest_memory(0x10_000);
        let table = GuestAddress(0x1000);

        write_descriptor(
            &mem,
            table,
            0,
            Descriptor {
                addr: 0x4000,
                len: 4,
                flags: 1,
                next: 1,
            },
        );
        write_descriptor(
            &mem,
            table,
            1,
            Descriptor {
                addr: 0x5000,
                len: 4,
                flags: 0,
                next: 0,
            },
        );

        let chain = DescriptorChain::new(&mem, table, 2, 0).unwrap();
        chain.write_at(2, b"abcd").unwrap();

        let mut buf = [0u8; 4];
        mem.read_slice(&mut buf, GuestAddress(0x4000 + 2)).unwrap();
        assert_eq!(&buf[..2], b"ab");
        mem.read_slice(&mut buf, GuestAddress(0x5000)).unwrap();
        assert_eq!(&buf[..2], b"cd");
    }

    // Empty writes no-op, overflow errors.
    #[test]
    fn descriptor_chain_write_at_edges() {
        let mem = guest_memory(0x10_000);
        let table = GuestAddress(0x1000);

        write_descriptor(
            &mem,
            table,
            0,
            Descriptor {
                addr: 0x4000,
                len: 4,
                flags: 0,
                next: 0,
            },
        );

        let chain = DescriptorChain::new(&mem, table, 1, 0).unwrap();
        assert!(chain.clone().write_at(0, b"").is_ok());

        let mut buf = [0u8; 4];
        mem.read_slice(&mut buf, GuestAddress(0x4000)).unwrap();
        assert_eq!(buf, [0, 0, 0, 0]);

        let chain = DescriptorChain::new(&mem, table, 1, 0).unwrap();
        assert!(chain.write_at(5, b"x").is_err());
    }

    // Valid queue layout accepted.
    #[test]
    fn queue_is_valid_accepts_well_formed_layout() {
        let (queue, mem) = valid_queue();

        assert!(queue.is_valid(&mem));
    }

    // Invalid queue layout rejected.
    #[test]
    fn queue_is_valid_rejects_bad_layouts() {
        let (mut queue, mem) = valid_queue();

        queue.ready = false;
        assert!(!queue.is_valid(&mem));

        queue.ready = true;
        queue.size = 3;
        assert!(!queue.is_valid(&mem));

        queue.size = 16;
        assert!(!queue.is_valid(&mem));

        queue.size = 8;
        queue.desc_table = GuestAddress(0x1003);
        assert!(!queue.is_valid(&mem));

        queue.desc_table = GuestAddress(0x1000);
        queue.avail_ring = GuestAddress(0x2002);
        assert!(!queue.is_valid(&mem));

        queue.avail_ring = GuestAddress(0x2000);
        queue.used_ring = GuestAddress(0x3001);
        assert!(!queue.is_valid(&mem));
    }

    // Pop reads avail ring.
    #[test]
    fn queue_pop_reads_avail_ring() {
        let (mut queue, mem) = valid_queue();
        write_descriptor(
            &mem,
            queue.desc_table,
            0,
            Descriptor {
                addr: 0x4000,
                len: 8,
                flags: 0,
                next: 0,
            },
        );
        write_u16(&mem, queue.avail_ring, 0);
        write_u16(&mem, queue.avail_ring.unchecked_add(2), 1);
        write_u16(&mem, queue.avail_ring.unchecked_add(4), 0);

        let desc = queue.pop(&mem).unwrap().expect("descriptor");
        assert_eq!(desc.index, 0);
        assert_eq!(queue.next_avail.0, 1);
    }

    // Empty queue yields none.
    #[test]
    fn queue_pop_returns_none_when_empty() {
        let (mut queue, mem) = valid_queue();

        assert!(queue.pop(&mem).unwrap().is_none());
    }

    // Bad descriptor keeps index still.
    #[test]
    fn queue_pop_invalid_descriptor_does_not_advance() {
        let (mut queue, mem) = valid_queue();

        write_u16(&mem, queue.avail_ring, 0);
        write_u16(&mem, queue.avail_ring.unchecked_add(2), 1);
        write_u16(&mem, queue.avail_ring.unchecked_add(4), 99);

        assert!(queue.pop(&mem).is_err());
        assert_eq!(queue.next_avail.0, 0);
    }

    // Undo pop steps back one slot.
    #[test]
    fn queue_undo_pop_rewinds_index() {
        let (mut queue, _mem) = valid_queue();

        queue.next_avail = std::num::Wrapping(2);
        queue.undo_pop();

        assert_eq!(queue.next_avail.0, 1);
    }

    // Push writes used ring entry.
    #[test]
    fn queue_push_writes_used_entry() {
        let (mut queue, mem) = valid_queue();

        queue.push(&mem, 7, 11);

        let mut id = [0u8; 4];
        let mut len = [0u8; 4];
        mem.read_slice(&mut id, queue.used_ring.unchecked_add(4))
            .unwrap();
        mem.read_slice(&mut len, queue.used_ring.unchecked_add(8))
            .unwrap();

        assert_eq!(u32::from_le_bytes(id), 7);
        assert_eq!(u32::from_le_bytes(len), 11);
        assert_eq!(queue.next_used.0, 1);
        assert_eq!(queue.num_added.0, 1);
    }

    // Notify checks avail flags.
    #[test]
    fn queue_prepare_notify_without_suppression_checks_flags() {
        let (mut queue, mem) = valid_queue();

        write_u16(&mem, queue.avail_ring, 0);
        assert!(queue.prepare_notify(&mem));

        write_u16(&mem, queue.avail_ring, 1);
        assert!(!queue.prepare_notify(&mem));
    }

    // Suppress mode uses event idx.
    #[test]
    fn queue_prepare_notify_with_suppression_uses_event_idx() {
        let (mut queue, mem) = valid_queue();

        queue.notif_suppress = true;
        queue.next_used = std::num::Wrapping(1);
        queue.num_added = std::num::Wrapping(1);
        write_u16(&mem, queue.avail_ring.unchecked_add(2 * 8 + 4), 0);

        assert!(queue.prepare_notify(&mem));
        assert_eq!(queue.num_added.0, 0);
    }
}
