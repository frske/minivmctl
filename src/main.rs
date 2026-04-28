use std::{
    fs::File,
    io::{self, Cursor},
    num::Wrapping,
    ops::Deref,
    sync::{
        atomic::{fence, AtomicU32, Ordering},
        Arc, Mutex,
    },
};

// mod blog;

use kvm_bindings::{
    kvm_pit_config, kvm_segment, kvm_userspace_memory_region, KVM_MAX_CPUID_ENTRIES,
    KVM_MEM_LOG_DIRTY_PAGES, KVM_PIT_SPEAKER_DUMMY,
};
use kvm_ioctls::{Kvm, VcpuExit};
use libc::SYS_sched_get_priority_max;
use linux_loader::{
    bootparam::boot_params,
    configurator::{linux::LinuxBootConfigurator, BootConfigurator, BootParams},
    loader::{load_cmdline, Cmdline, Elf, KernelLoader},
};
use vm_memory::{Address, ByteValued, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};
use vm_superio::{serial::SerialEvents, Serial, Trigger};
use vmm_sys_util::{
    eventfd::{EventFd, EFD_NONBLOCK},
    fallocate,
    poll::{PollContext, PollEvents},
    terminal::Terminal,
};

const MEMORY_SIZE: usize = 128 << 20;
const BOOT_GDT_OFFSET: u64 = 0x500;
const X86_CR0_PG: u64 = 0x80000000;
const X86_CR4_PAE: u64 = 0x20;
const X86_CR0_PE: u64 = 0x1;
const EFER_LME: u64 = 0x100;
const EFER_LMA: u64 = 0x400;
const KVM_TSS_ADDRESS: usize = 0xfffb_d000;

const KERNEL_TYPE_OF_LOADER: u8 = 0xff;
const KERNEL_BOOT_FLAG_MAGIC_NUMBER: u16 = 0xaa55;
const KERNEL_HDR_MAGIC_NUMBER: u32 = 0x5372_6448;
const KERNEL_MIN_ALIGNMENT_BYTES: u32 = 0x0100_0000;

// Value taken from <https://elixir.bootlin.com/linux/v5.10.68/source/arch/x86/include/uapi/asm/e820.h#L31>
const E820_RAM: u32 = 1;
const EBDA_START: u64 = 0x9fc00;
const FIRST_ADDR_PAST_32BITS: u64 = 1 << 32;
const MEM_32BIT_GAP_SIZE: u64 = 768 << 20;
const MMIO_MEM_START: u64 = FIRST_ADDR_PAST_32BITS - MEM_32BIT_GAP_SIZE;

const COM1: u16 = 0x3f8;

const BOOT_CMD: &str = "console=ttyS0 noapic noacpi reboot=k panic=1 pci=off nomodule";
const KERNEL_PATH: &str = "/tmp/chinsav/vmlinux";
const INITRD_PATH: &str = "/tmp/chinsav/initrd.img";

const HIMEM_START: u64 = 0x100000;
const BOOT_CMD_START: u64 = 0x20000;
const ZERO_PAGE_START: u64 = 0x7000;
const BOOT_STACK_POINTER: u64 = 0x8ff0;

const MMIO_MAGIC_VALUE: u32 = 0x7472_6976;
const MMIO_VERSION: u32 = 2;
const VENDOR_ID: u32 = 0;

const NET_NUM_QUEUES: usize = 2;
const VIRTIO_TYPE_NET: u32 = 1;

fn main() {
    // VM and guest memory setup.
    let kvm = Kvm::new().expect("Open kvm device failed");
    let vm = kvm.create_vm().expect("create vm failed");

    let boot_pte_addr = GuestAddress(0xc000);
    let boot_pml4_addr = GuestAddress(0xa000);
    let boot_pdpte_addr = GuestAddress(0xb000);
    let boot_pde_addr = GuestAddress(0xc000);

    let guest_addr = GuestAddress(0x0);
    let guest_mem = GuestMemoryMmap::<()>::from_ranges(&[(guest_addr, MEMORY_SIZE)]).unwrap();

    let kernel_entry = load_kernel(&guest_mem);

    register_guest_memory(&vm, &guest_mem, guest_addr);
    write_page_tables(
        &guest_mem,
        boot_pdpte_addr,
        boot_pde_addr,
        boot_pte_addr,
        boot_pml4_addr,
    );
    configure_irq_and_pit(&vm);

    let mut vcpu = vm.create_vcpu(0).expect("Failed creating vcpu");
    configure_vcpu(&kvm, &mut vcpu, &kernel_entry);

    // Guest CPU state and boot data.
    const CODE_SEG: kvm_segment = seg_with_st(1, 0b1011);
    const DATA_SEG: kvm_segment = seg_with_st(2, 0b0011);

    let mut sregs = configure_segments_and_paging(&vcpu, boot_pml4_addr, CODE_SEG, DATA_SEG);
    write_gdt(&guest_mem, &mut sregs, CODE_SEG, DATA_SEG);
    vcpu.set_sregs(&sregs).unwrap();

    let (initrd_addr, initrd_size) = load_initrd(&guest_mem);
    load_boot_cmdline(&guest_mem);
    write_boot_params(&guest_mem, initrd_addr, initrd_size);

    // Runtime plumbing.
    let com_evt_1 = EventWrapper::new();
    vm.register_irqfd(&com_evt_1.0, 4).unwrap();

    let stdio_serial = Arc::new(Mutex::new(Serial::with_events(
        com_evt_1.try_clone().unwrap(),
        DummySerialEvent,
        std::io::stdout(),
    )));

    let _ = kernel_entry;

    let exit_evt = EventWrapper::new();
    spawn_vcpu_runtime(vcpu, stdio_serial.clone(), exit_evt.try_clone().unwrap());

    let stdin = std::io::stdin().lock();
    stdin.set_raw_mode().expect("failed to set raw mode");

    let poll: PollContext<u8> = PollContext::new().unwrap();
    poll.add(&exit_evt.0, 0).unwrap();
    poll.add(&stdin, 1).unwrap();

    'l: loop {
        let events: PollEvents<u8> = poll.wait().unwrap();
        for event in events.iter_readable() {
            match event.token() {
                0 => {
                    println!("vcpu stopped, main loop exit");
                    break 'l;
                }
                1 => {
                    let mut out = [0u8; 64];
                    match stdin.read_raw(&mut out[..]) {
                        Ok(0) => {}
                        Ok(count) => {
                            stdio_serial
                                .lock()
                                .unwrap()
                                .enqueue_raw_bytes(&out[..count])
                                .expect("failed to enqueue bytes");
                        }
                        Err(e) => {
                            println!("error while reading stdin: {:?}", e);
                        }
                    }
                }
                _ => unreachable!(),
            }
        }
    }
}

fn spawn_vcpu_runtime(
    mut vcpu: kvm_ioctls::VcpuFd,
    stdio_serial: Arc<Mutex<Serial<EventWrapper, DummySerialEvent, std::io::Stdout>>>,
    vcpu_exit_evt: EventWrapper,
) {
    std::thread::spawn(move || {
        loop {
            match vcpu.run() {
                Ok(run) => match run {
                    VcpuExit::IoIn(addr, data) => {
                        if addr >= COM1 && addr - COM1 < 8 {
                            data[0] = stdio_serial.lock().unwrap().read((addr - COM1) as u8)
                        }
                    }
                    VcpuExit::IoOut(addr, data) => {
                        if addr >= COM1 && addr - COM1 < 8 {
                            let _ = stdio_serial
                                .lock()
                                .unwrap()
                                .write((addr - COM1) as u8, data[0]);
                        }
                    }
                    VcpuExit::MmioRead(_, _) => {}
                    VcpuExit::MmioWrite(_, _) => {}
                    VcpuExit::Hlt => {
                        println!("KVM_EXIT_HLT");
                        break;
                    }
                    VcpuExit::Shutdown => {
                        println!("KVM_EXIT_SHIUTDOWN");
                        break;
                    }
                    r => {
                        println!("KVM_EXIT: {:?}", r)
                    }
                },
                Err(e) => {
                    println!("KVM Run error {:?}", e);
                    break;
                }
            }
        }
        vcpu_exit_evt.trigger().unwrap();
    });
}

fn register_guest_memory(
    vm: &kvm_ioctls::VmFd,
    guest_mem: &GuestMemoryMmap<()>,
    guest_addr: GuestAddress,
) {
    let host_addr = guest_mem.get_host_address(guest_addr).unwrap();
    let mem_region = kvm_userspace_memory_region {
        slot: 0,
        guest_phys_addr: 0,
        memory_size: MEMORY_SIZE as u64,
        userspace_addr: host_addr as u64,
        flags: KVM_MEM_LOG_DIRTY_PAGES,
    };

    unsafe {
        vm.set_user_memory_region(mem_region)
            .expect("set user memory region failed");
    }
}

fn write_page_tables(
    guest_mem: &GuestMemoryMmap<()>,
    boot_pdpte_addr: GuestAddress,
    boot_pde_addr: GuestAddress,
    _boot_pte_addr: GuestAddress,
    boot_pml4_addr: GuestAddress,
) {
    guest_mem
        .write_slice(
            &(boot_pdpte_addr.raw_value() as u64 | 1).to_le_bytes(),
            boot_pml4_addr,
        )
        .unwrap();

    guest_mem
        .write_slice(
            &(boot_pde_addr.raw_value() as u64 | 0b11).to_le_bytes(),
            boot_pdpte_addr,
        )
        .unwrap();

    for i in 0..512 {
        guest_mem
            .write_slice(
                &((i << 21) | 0b10000011u64).to_le_bytes(),
                boot_pde_addr.unchecked_add(i * 8),
            )
            .unwrap()
    }
}

fn configure_vcpu(kvm: &Kvm, vcpu: &mut kvm_ioctls::VcpuFd, kernel_entry: &GuestAddress) {
    let kvm_cpuid = kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES).unwrap();

    vcpu.set_cpuid2(&kvm_cpuid).unwrap();

    let mut regs = vcpu.get_regs().unwrap();
    regs.rip = kernel_entry.raw_value();
    regs.rsp = BOOT_STACK_POINTER;
    regs.rbp = BOOT_STACK_POINTER;
    regs.rsi = ZERO_PAGE_START;
    regs.rflags = 2;
    vcpu.set_regs(&regs).unwrap();
}

fn configure_irq_and_pit(vm: &kvm_ioctls::VmFd) {
    vm.create_irq_chip().unwrap();
    // vm.register_irqfd_with_resample(fd, resamplefd, gsi)
    // .unwrap();

    let pit_config = kvm_pit_config {
        flags: KVM_PIT_SPEAKER_DUMMY,
        ..Default::default()
    };
    vm.create_pit2(pit_config).unwrap();
    vm.set_tss_address(KVM_TSS_ADDRESS as usize)
        .expect("failed to set tss");
}

fn configure_segments_and_paging(
    vcpu: &kvm_ioctls::VcpuFd,
    boot_pml4_addr: GuestAddress,
    code_seg: kvm_segment,
    data_seg: kvm_segment,
) -> kvm_bindings::kvm_sregs {
    let mut sregs = vcpu.get_sregs().unwrap();

    sregs.cs = code_seg;
    sregs.ds = data_seg;
    sregs.es = data_seg;
    sregs.fs = data_seg;
    sregs.gs = data_seg;
    sregs.ss = data_seg;

    sregs.cr3 = boot_pml4_addr.raw_value() as u64;
    sregs.cr4 |= X86_CR4_PAE;
    sregs.cr0 |= X86_CR0_PE | X86_CR0_PG;
    sregs.efer |= EFER_LMA | EFER_LME;

    sregs
}

fn write_gdt(
    guest_mem: &GuestMemoryMmap<()>,
    sregs: &mut kvm_bindings::kvm_sregs,
    code_seg: kvm_segment,
    data_seg: kvm_segment,
) {
    let gdt_table: [u64; 3] = [0, to_gdt_entry(&code_seg), to_gdt_entry(&data_seg)];
    let boot_gdt_address = GuestAddress(BOOT_GDT_OFFSET);

    for (index, entry) in gdt_table.iter().enumerate() {
        let addr = guest_mem
            .checked_offset(boot_gdt_address, index * std::mem::size_of::<u64>())
            .unwrap();
        guest_mem.write_obj(*entry, addr).unwrap();
    }

    sregs.gdt.base = BOOT_GDT_OFFSET;
    sregs.gdt.limit = std::mem::size_of_val(&gdt_table) as u16 - 1;
}

fn load_kernel(guest_mem: &GuestMemoryMmap<()>) -> GuestAddress {
    let mut kernel_file = File::open(KERNEL_PATH).expect("failed to open kernel path");
    Elf::load(
        guest_mem,
        None,
        &mut kernel_file,
        Some(GuestAddress(HIMEM_START)),
    )
    .unwrap()
    .kernel_load
}

fn load_initrd(guest_mem: &GuestMemoryMmap<()>) -> (GuestAddress, usize) {
    let initrd_content = std::fs::read(INITRD_PATH).expect("failed to read initrd");
    let first_region = guest_mem.find_region(GuestAddress::new(0)).unwrap();
    assert!(
        initrd_content.len() <= first_region.size(),
        "too big initrd"
    );

    let initrd_addr =
        GuestAddress((first_region.size() - initrd_content.len()) as u64 & !(4096 - 1));
    guest_mem
        .read_volatile_from(
            initrd_addr,
            &mut Cursor::new(&initrd_content),
            initrd_content.len(),
        )
        .unwrap();

    (initrd_addr, initrd_content.len())
}

fn load_boot_cmdline(guest_mem: &GuestMemoryMmap<()>) {
    let mut boot_cmdline = Cmdline::new(0x10000).unwrap();
    boot_cmdline.insert_str(BOOT_CMD).unwrap();
    load_cmdline(guest_mem, GuestAddress(BOOT_CMD_START), &boot_cmdline).unwrap();
}

fn write_boot_params(
    guest_mem: &GuestMemoryMmap<()>,
    initrd_addr: GuestAddress,
    initrd_size: usize,
) {
    let mut params = boot_params::default();

    params.hdr.type_of_loader = KERNEL_TYPE_OF_LOADER;
    params.hdr.boot_flag = KERNEL_BOOT_FLAG_MAGIC_NUMBER;
    params.hdr.header = KERNEL_HDR_MAGIC_NUMBER;
    params.hdr.cmd_line_ptr = BOOT_CMD_START as u32;
    params.hdr.cmdline_size = 1 + BOOT_CMD.len() as u32;
    params.hdr.kernel_alignment = KERNEL_MIN_ALIGNMENT_BYTES;
    params.hdr.ramdisk_image = initrd_addr.raw_value() as u32;
    params.hdr.ramdisk_size = initrd_size as u32;

    add_e820_entry(&mut params, 0, EBDA_START, E820_RAM);
    let last_addr = guest_mem.last_addr();
    let first_addr_past_32bits = GuestAddress(FIRST_ADDR_PAST_32BITS);
    let end_32bit_gap_start = GuestAddress(MMIO_MEM_START);
    let himem_start = GuestAddress(HIMEM_START);

    if last_addr < end_32bit_gap_start {
        add_e820_entry(
            &mut params,
            himem_start.raw_value() as u64,
            last_addr.unchecked_offset_from(himem_start) as u64 + 1,
            E820_RAM,
        );
    } else {
        add_e820_entry(
            &mut params,
            himem_start.raw_value() as u64,
            end_32bit_gap_start.unchecked_offset_from(himem_start),
            E820_RAM,
        );

        if last_addr > first_addr_past_32bits {
            add_e820_entry(
                &mut params,
                first_addr_past_32bits.raw_value(),
                last_addr.unchecked_offset_from(first_addr_past_32bits) + 1,
                E820_RAM,
            );
        }
    }

    LinuxBootConfigurator::write_bootparams(
        &BootParams::new(&params, GuestAddress(ZERO_PAGE_START)),
        guest_mem,
    )
    .unwrap();
}

const fn seg_with_st(selector_index: u16, type_: u8) -> kvm_segment {
    kvm_segment {
        base: 0,
        limit: 0x000fffff,
        selector: selector_index << 3,
        type_,
        present: 1,
        dpl: 0,
        db: 0,
        s: 1,
        l: 1,
        g: 1,
        avl: 0,
        unusable: 0,
        padding: 0,
    }
}

// Ref: <https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html> 3-10 Vol. 3A
const fn to_gdt_entry(seg: &kvm_segment) -> u64 {
    let base = seg.base;
    let limit = seg.limit as u64;
    // flags: G, DB, L, AVL
    let flags = (seg.g as u64 & 0x1) << 3
        | (seg.db as u64 & 0x1) << 2
        | (seg.l as u64 & 0x1) << 1
        | (seg.avl as u64 & 0x1);
    // access: P, DPL, S, Type
    let access = (seg.present as u64 & 0x1) << 7
        | (seg.dpl as u64 & 0x11) << 5
        | (seg.s as u64 & 0x1) << 4
        | (seg.type_ as u64 & 0x1111);
    ((base & 0xff00_0000u64) << 32)
        | ((base & 0x00ff_ffffu64) << 16)
        | (limit & 0x0000_ffffu64)
        | ((limit & 0x000f_0000u64) << 32)
        | (flags << 52)
        | (access << 40)
}

fn add_e820_entry(params: &mut boot_params, addr: u64, size: u64, mem_type: u32) {
    if params.e820_entries >= params.e820_table.len() as u8 {
        panic!();
    }
    params.e820_table[params.e820_entries as usize].addr = addr;
    params.e820_table[params.e820_entries as usize].size = size;
    params.e820_table[params.e820_entries as usize].type_ = mem_type;
    params.e820_entries += 1;
}

struct EventWrapper(EventFd);

impl EventWrapper {
    pub fn new() -> Self {
        Self(EventFd::new(EFD_NONBLOCK).unwrap())
    }

    pub fn try_clone(&self) -> std::io::Result<Self> {
        self.0.try_clone().map(Self)
    }
}

impl std::ops::Deref for EventWrapper {
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

struct DummySerialEvent;

impl SerialEvents for DummySerialEvent {
    fn buffer_read(&self) {}
    fn out_byte(&self) {}
    fn tx_lost_byte(&self) {}
    fn in_buffer_empty(&self) {}
}

pub struct DescriptorChain<M> {
    mem: M,
    desc_table: GuestAddress,
    queue_size: u16,
    pub index: u16,

    pub addr: u64,  //memroy address in Guest
    pub len: u32,   //length of buffer
    pub flags: u16, //readable, writable, has next
    pub next: u16,  //next descriptor if index chained
}

impl<M: Deref<Target = impl GuestMemoryBackend>> DescriptorChain<M> {
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

    pub fn next(&self) -> Option<Result<Self, ()>>
    where
        M: Clone,
    {
        const VIRTQ_DESC_F_NEXT: u16 = 0x1;
        if self.flags & VIRTQ_DESC_F_NEXT != 0 {
            Some(Self::new(
                self.mem.clone(),
                self.desc_table,
                self.queue_size,
                self.next,
            ))
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
            next_desc = desc.next().transpose().map_err(|_| {
                io::Error::new(io::ErrorKind::Other, "Failed to get next descriptor")
            })?;
        }
        Err(io::Error::new(
            io::ErrorKind::Other,
            "Failed to write slice",
        ))
    }
}

pub struct Queue {
    pub desc_table: GuestAddress,
    pub avail_ring: GuestAddress,
    pub used_ring: GuestAddress,

    pub size: u16,
    pub ready: bool,

    pub next_avail: Wrapping<u16>,
    pub next_used: Wrapping<u16>,
    pub num_added: Wrapping<u16>,

    pub max_size: u16,
    pub notif_suppress: bool,
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

        unsafe impl ByteValued for UsedElem {} //understand this thoroughly

        // let slot = self.next_used.0 % self.size;
        // let offset = 4 + u64::from(slot) * 8;
        // let addr = self.used_ring.unchecked_add(offset);

        let addr = self
            .used_ring
            .unchecked_add((((self.next_used.0 % self.size) as u64) << 3) + 4);
        let id = desc_index as u32;
        mem.write_obj(UsedElem { id, len }, addr).unwrap();

        self.num_added += 1;
        self.next_avail += 1;

        fence(Ordering::Release);

        mem.write_obj(self.next_used.0, self.used_ring.unchecked_add(2))
            .unwrap();
    }

    pub fn prepare_notify<M: GuestMemoryBackend>(&mut self, mem: &M) -> bool {
        if !self.notif_suppress {
            let flags = mem.read_obj::<u16>(self.avail_ring).unwrap();
            return flags == 0;
        }

        fence(Ordering::AcqRel);

        let used_event_addr = self.avail_ring.unchecked_add(((self.size as u64) << 1) + 4);
        let used_event = Wrapping(mem.read_obj::<u16>(used_event_addr).unwrap());
        let num_added = self.num_added;
        self.num_added = Wrapping(0);

        self.next_used - used_event - Wrapping(1) < num_added
    }

    // Alternative:

    // pub fn validate<M: GuestMemory>(&self, mem: &M) -> Result<(), QueueValidationError>

    // That would be more diagnosable.

    pub fn is_valid<M: GuestMemoryBackend>(&self, mem: &M) -> bool {
        if !self.ready {
            return false;
        }

        if self.size > self.max_size {
            return false;
        }

        // if self.size == 0 || !self.size.is_power_of_two() || self.size > 32768 {

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

    //TODO: improve for safety all down here
    fn pop_unchecked<M: Deref<Target = impl GuestMemoryBackend>>(
        &mut self,
        mem: M,
    ) -> Result<DescriptorChain<M>, ()> {
        // let ring_index = self.next_avail.0 % self.size;
        // let offset = 4 + u64::from(ring_index) * 2;
        // let addr = self.avail_ring.unchecked_add(offset);

        let addr = self
            .avail_ring
            .unchecked_add((((self.next_avail.0 % self.size) as u64) * 2) + 4); //*4 should be equivalent to << 4
        let desc_index = mem.read_obj::<u16>(addr).unwrap();
        let desc_chain = DescriptorChain::new(mem, self.desc_table, self.size, desc_index);

        if desc_chain.is_ok() {
            self.next_avail += 1;
        } else {
            eprintln!("pop failed because invalid desc index"); //do not use this but use "structual loggin macro"
        }
        desc_chain
    }

    #[inline(always)]
    fn avail_idx<M: GuestMemoryBackend>(&self, mem: &M) -> Wrapping<u16> {
        let addr = self.avail_ring.unchecked_add(2);
        Wrapping(mem.read_obj::<u16>(addr).unwrap())
    }

    #[inline(always)]
    fn len<M: GuestMemoryBackend>(&self, mem: &M) -> u16 {
        (self.avail_idx(mem) - self.next_avail).0
    }
}

// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

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

    fn interrupt_trigger(&self) -> &Trigger; // IrqTrigger is what
    fn interrupt_status(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.interrupt_trigger().tr) //todo
    }

    fn avail_features_by_page(&self, page: u32) -> u32 {
        let avail_features = self.avail_features();

        match page {
            0 => (avail_features & 0xffff_ffff) as u32, //this bit mask is not needed in theory
            1 => (avail_features >> 32) as u32,
            _ => 0u32,
        }
    }

    fn ack_features_by_page(&mut self, page: u32, value: u32) {
        let mut v = match page {
            0 => u64::from(value),
            1 => u64::from(value) << 32,
            _ => 0u64,
        };

        //can be simplifed a lot
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
