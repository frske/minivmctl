use std::{
    collections::VecDeque,
    fs::File,
    io::Cursor,
    sync::{Arc, Mutex},
    time::Duration,
};

mod event;
mod mmio;
mod net;
mod serial;
mod virtio;
mod bus;

use kvm_bindings::{
    KVM_MAX_CPUID_ENTRIES, KVM_MEM_LOG_DIRTY_PAGES, KVM_PIT_SPEAKER_DUMMY, kvm_pit_config,
    kvm_segment, kvm_userspace_memory_region,
};
use kvm_ioctls::{IoEventAddress, Kvm, VcpuExit};
use linux_loader::{
    bootparam::boot_params,
    configurator::{BootConfigurator, BootParams, linux::LinuxBootConfigurator},
    loader::{Cmdline, Elf, KernelLoader, load_cmdline},
};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};
use vm_superio::{Serial, Trigger};
use vmm_sys_util::{
    poll::{PollContext, PollEvents},
    terminal::Terminal,
};

use crate::{
    bus::MMIOBus,
    event::EventWrapper,
    serial::DummySerialEvent,
    virtio::VirtioDevice,
};

type HostSerial = Serial<EventWrapper, DummySerialEvent, std::io::Stdout>;

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

const BOOT_CMD: &str =
    "console=ttyS0 noapic noacpi reboot=k panic=1 pci=off net.ifnames=0 biosdevname=0 virtio_mmio.device=4K@0xd0000000:5";
const KERNEL_PATH: &str = "/tmp/chinsav/vmlinux";
const INITRD_PATH: &str = "/tmp/chinsav/initrd.img";

const HIMEM_START: u64 = 0x100000;
const BOOT_CMD_START: u64 = 0x20000;
const ZERO_PAGE_START: u64 = 0x7000;
const BOOT_STACK_POINTER: u64 = 0x8ff0;
const QUEUE_NOTIFY_OFFSET: u64 = 0x50;
const GUEST_MAC: [u8; 6] = [0x02, 0x12, 0x34, 0x56, 0x78, 0x9a];

const NET_NUM_QUEUES: usize = 2;
const TAP_RX_TOKEN: u8 = 3 + NET_NUM_QUEUES as u8;

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
    let mut mmio_bus = MMIOBus::new();

    let virtio_net = net::Net::new("tap0", Some(GUEST_MAC)).expect("failed to create virtio net");
    vm.register_irqfd(virtio_net.interrupt_event(), 5).unwrap();
    for (q_sel, evt) in virtio_net.queues_events().iter().enumerate() {
        let addr = IoEventAddress::Mmio(QUEUE_NOTIFY_OFFSET + MMIO_MEM_START);
        vm.register_ioevent(evt, &addr, q_sel as u32).unwrap();
    }

    let virtio_net_activate_evt = virtio_net.activate_event.try_clone().unwrap();
    let virtio_net_queue_events = virtio_net
        .queues_events()
        .iter()
        .map(|evt| evt.try_clone().unwrap())
        .collect::<Vec<_>>();
    let virtio_net = Arc::new(Mutex::new(virtio_net));
    mmio_bus
        .insert_virtio(MMIO_MEM_START, virtio_net.clone(), guest_mem.clone())
        .unwrap();

    let com_evt_1 = EventWrapper::new();
    vm.register_irqfd(&com_evt_1.0, 4).unwrap();

    let stdio_serial = Arc::new(Mutex::new(Serial::with_events(
        com_evt_1.try_clone().unwrap(),
        DummySerialEvent,
        std::io::stdout(),
    )));

    let _ = kernel_entry;

    let exit_evt = EventWrapper::new();
    spawn_vcpu_runtime(vcpu, stdio_serial.clone(), mmio_bus, exit_evt.try_clone().unwrap());

    let stdin = std::io::stdin().lock();
    stdin.set_raw_mode().expect("failed to set raw mode");

    let poll: PollContext<u8> = PollContext::new().unwrap();
    poll.add(&exit_evt.0, 0).unwrap();
    poll.add(&stdin, 1).unwrap();
    poll.add(&virtio_net_activate_evt, 2).unwrap();
    for (q_sel, evt) in virtio_net_queue_events.iter().enumerate() {
        poll.add(evt, 3 + q_sel as u8).unwrap();
    }

    let mut tap_rx_registered = false;
    let mut pending_stdin = VecDeque::new();

    'l: loop {
        flush_pending_stdin(&stdio_serial, &mut pending_stdin);

        let events: PollEvents<u8> = if pending_stdin.is_empty() {
            poll.wait().unwrap()
        } else {
            poll.wait_timeout(Duration::from_millis(10)).unwrap()
        };
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
                            pending_stdin.extend(&out[..count]);
                            flush_pending_stdin(&stdio_serial, &mut pending_stdin);
                        }
                        Err(e) => {
                            println!("error while reading stdin: {:?}", e);
                        }
                    }
                }
                2 => {
                    let _ = virtio_net_activate_evt.read().unwrap();
                    if !tap_rx_registered {
                        virtio_net
                            .lock()
                            .unwrap()
                            .register_tap_readable(&poll, TAP_RX_TOKEN)
                            .unwrap();
                        tap_rx_registered = true;
                    }
                }
                TAP_RX_TOKEN => {
                    let mut net = virtio_net.lock().unwrap();
                    let _ = net.process_rx();
                }
                token if token >= 3 && token < 3 + NET_NUM_QUEUES as u8 => {
                    let idx = (token - 3) as usize;
                    let _ = virtio_net_queue_events[idx].read().unwrap();
                    let mut net = virtio_net.lock().unwrap();
                    let _ = net.process_tx();
                }
                _ => unreachable!(),
            }
        }
    }
}

fn spawn_vcpu_runtime(
    mut vcpu: kvm_ioctls::VcpuFd,
    stdio_serial: Arc<Mutex<HostSerial>>,
    mut mmio_bus: MMIOBus,
    vcpu_exit_evt: EventWrapper,
) {
    std::thread::spawn(move || {
        loop {
            match vcpu.run() {
                Ok(run) => match run {
                    VcpuExit::IoIn(addr, data) => {
                        if addr >= COM1 && addr - COM1 < 8 {
                            data[0] = lock_host_serial(&stdio_serial).read((addr - COM1) as u8)
                        }
                    }
                    VcpuExit::IoOut(addr, data) => {
                        if addr >= COM1 && addr - COM1 < 8 {
                            let _ = lock_host_serial(&stdio_serial).write((addr - COM1) as u8, data[0]);
                        }
                    }
                    VcpuExit::MmioRead(addr, data) => {
                        mmio_bus.read(addr, data);
                    }
                    VcpuExit::MmioWrite(addr, data) => {
                        mmio_bus.write(addr, data);
                    }
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

fn lock_host_serial(
    serial: &Arc<Mutex<HostSerial>>,
) -> std::sync::MutexGuard<'_, HostSerial> {
    match serial.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn flush_pending_stdin(serial: &Arc<Mutex<HostSerial>>, pending: &mut VecDeque<u8>) {
    loop {
        if pending.is_empty() {
            break;
        }

        let written = {
            let contiguous = pending.make_contiguous();
            let mut serial = lock_host_serial(serial);
            match serial.enqueue_raw_bytes(contiguous) {
                Ok(written) => written,
                Err(vm_superio::serial::Error::FullFifo) => break,
                Err(e) => {
                    println!("failed to enqueue bytes: {:?}", e);
                    break;
                }
            }
        };

        if written == 0 {
            break;
        }

        pending.drain(..written);
    }
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
