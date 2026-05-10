use std::fs::File;

use kvm_bindings::{
    KVM_MAX_CPUID_ENTRIES, KVM_MEM_LOG_DIRTY_PAGES, KVM_PIT_SPEAKER_DUMMY, kvm_pit_config,
    kvm_segment, kvm_userspace_memory_region,
};
use kvm_ioctls::{Kvm, VcpuFd, VmFd};
use linux_loader::{
    bootparam::boot_params,
    configurator::{BootConfigurator, BootParams, linux::LinuxBootConfigurator},
    loader::{Cmdline, Elf, KernelLoader, load_cmdline},
};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

pub const MEMORY_SIZE: usize = 512 << 20;

const PTE_PRESENT: u64 = 0x1;
const PTE_WRITABLE: u64 = 0x2;
const PTE_HUGE_PAGE: u64 = 0x80;
const PTE_PRESENT_WRITABLE: u64 = PTE_PRESENT | PTE_WRITABLE;
const PTE_PRESENT_WRITABLE_HUGE: u64 = PTE_PRESENT | PTE_WRITABLE | PTE_HUGE_PAGE;

pub const BOOT_GDT_OFFSET: u64 = 0x500;
pub const X86_CR0_PG: u64 = 0x80000000;
pub const X86_CR4_PAE: u64 = 0x20;
pub const X86_CR0_PE: u64 = 0x1;
pub const EFER_LME: u64 = 0x100;
pub const EFER_LMA: u64 = 0x400;
pub const KVM_TSS_ADDRESS: usize = 0xfffb_d000;

pub const KERNEL_TYPE_OF_LOADER: u8 = 0xff;
pub const KERNEL_BOOT_FLAG_MAGIC_NUMBER: u16 = 0xaa55;
pub const KERNEL_HDR_MAGIC_NUMBER: u32 = 0x5372_6448;
pub const KERNEL_MIN_ALIGNMENT_BYTES: u32 = 0x0100_0000;

pub const E820_RAM: u32 = 1;
pub const EBDA_START: u64 = 0x9fc00;
pub const FIRST_ADDR_PAST_32BITS: u64 = 1 << 32;
pub const MEM_32BIT_GAP_SIZE: u64 = 768 << 20;
pub const MMIO_MEM_START: u64 = FIRST_ADDR_PAST_32BITS - MEM_32BIT_GAP_SIZE;

pub const BOOT_CMD: &str = "console=ttyS0 noapic noacpi reboot=k panic=1 pci=off net.ifnames=0 biosdevname=0 root=/dev/vda rootwait rw virtio_mmio.device=4K@0xd0000000:5 virtio_mmio.device=4K@0xd0001000:6";
pub const HIMEM_START: u64 = 0x100000;
pub const BOOT_CMD_START: u64 = 0x20000;
pub const ZERO_PAGE_START: u64 = 0x7000;
pub const BOOT_STACK_POINTER: u64 = 0x8ff0;

pub struct BootArtifacts {
    pub vm: VmFd,
    pub vcpu: VcpuFd,
    pub guest_mem: GuestMemoryMmap<()>,
}

pub fn setup(kernel_path: &str) -> BootArtifacts {
    let kvm = Kvm::new().expect("Open kvm device failed");
    let vm = kvm.create_vm().expect("create vm failed");

    let boot_pte_addr = GuestAddress(0xc000);
    let boot_pml4_addr = GuestAddress(0xa000);
    let boot_pdpte_addr = GuestAddress(0xb000);
    let boot_pde_addr = GuestAddress(0xc000);

    let guest_addr = GuestAddress(0x0);
    let guest_mem = GuestMemoryMmap::<()>::from_ranges(&[(guest_addr, MEMORY_SIZE)]).unwrap();

    let kernel_entry = load_kernel(&guest_mem, kernel_path);

    register_guest_memory(&vm, &guest_mem);
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

    const CODE_SEG: kvm_segment = seg_with_st(1, 0b1011);
    const DATA_SEG: kvm_segment = seg_with_st(2, 0b0011);

    let mut sregs = configure_segments_and_paging(&vcpu, boot_pml4_addr, CODE_SEG, DATA_SEG);
    write_gdt(&guest_mem, &mut sregs, CODE_SEG, DATA_SEG);
    vcpu.set_sregs(&sregs).unwrap();

    load_boot_cmdline(&guest_mem);
    write_boot_params(&guest_mem);

    BootArtifacts {
        vm,
        vcpu,
        guest_mem,
    }
}

pub fn register_guest_memory(vm: &VmFd, guest_mem: &GuestMemoryMmap<()>) {
    let host_addr = guest_mem.get_host_address(GuestAddress(0)).unwrap();
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

pub fn write_page_tables(
    guest_mem: &GuestMemoryMmap<()>,
    boot_pdpte_addr: GuestAddress,
    boot_pde_addr: GuestAddress,
    _boot_pte_addr: GuestAddress,
    boot_pml4_addr: GuestAddress,
) {
    guest_mem
        .write_slice(
            &(boot_pdpte_addr.raw_value() | PTE_PRESENT).to_le_bytes(),
            boot_pml4_addr,
        )
        .unwrap();

    guest_mem
        .write_slice(
            &(boot_pde_addr.raw_value() | PTE_PRESENT_WRITABLE).to_le_bytes(),
            boot_pdpte_addr,
        )
        .unwrap();

    for i in 0..512 {
        guest_mem
            .write_slice(
                &((i << 21) | PTE_PRESENT_WRITABLE_HUGE).to_le_bytes(),
                boot_pde_addr.unchecked_add(i * 8),
            )
            .unwrap()
    }
}

pub fn configure_vcpu(kvm: &Kvm, vcpu: &mut VcpuFd, kernel_entry: &GuestAddress) {
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

pub fn configure_irq_and_pit(vm: &VmFd) {
    vm.create_irq_chip().unwrap();

    let pit_config = kvm_pit_config {
        flags: KVM_PIT_SPEAKER_DUMMY,
        ..Default::default()
    };
    vm.create_pit2(pit_config).unwrap();
    vm.set_tss_address(KVM_TSS_ADDRESS as usize)
        .expect("failed to set tss");
}

pub fn configure_segments_and_paging(
    vcpu: &VcpuFd,
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

    sregs.cr3 = boot_pml4_addr.raw_value();
    sregs.cr4 |= X86_CR4_PAE;
    sregs.cr0 |= X86_CR0_PE | X86_CR0_PG;
    sregs.efer |= EFER_LMA | EFER_LME;

    sregs
}

pub fn write_gdt(
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

pub fn load_kernel(guest_mem: &GuestMemoryMmap<()>, kernel_path: &str) -> GuestAddress {
    let mut kernel_file = File::open(kernel_path).expect("failed to open kernel path");
    Elf::load(
        guest_mem,
        None,
        &mut kernel_file,
        Some(GuestAddress(HIMEM_START)),
    )
    .unwrap()
    .kernel_load
}

pub fn load_boot_cmdline(guest_mem: &GuestMemoryMmap<()>) {
    let mut boot_cmdline = Cmdline::new(0x10000).unwrap();
    boot_cmdline.insert_str(BOOT_CMD).unwrap();
    load_cmdline(guest_mem, GuestAddress(BOOT_CMD_START), &boot_cmdline).unwrap();
}

pub fn write_boot_params(guest_mem: &GuestMemoryMmap<()>) {
    let mut params = boot_params::default();

    params.hdr.type_of_loader = KERNEL_TYPE_OF_LOADER;
    params.hdr.boot_flag = KERNEL_BOOT_FLAG_MAGIC_NUMBER;
    params.hdr.header = KERNEL_HDR_MAGIC_NUMBER;
    params.hdr.cmd_line_ptr = BOOT_CMD_START as u32;
    params.hdr.cmdline_size = 1 + BOOT_CMD.len() as u32;
    params.hdr.kernel_alignment = KERNEL_MIN_ALIGNMENT_BYTES;

    add_e820_entry(&mut params, 0, EBDA_START, E820_RAM);

    let last_addr = guest_mem.last_addr();
    let first_addr_past_32bits = GuestAddress(FIRST_ADDR_PAST_32BITS);
    let end_32bit_gap_start = GuestAddress(MMIO_MEM_START);
    let himem_start = GuestAddress(HIMEM_START);

    let himem_raw = himem_start.raw_value();
    let gap_size = end_32bit_gap_start.unchecked_offset_from(himem_start);
    add_e820_entry(&mut params, himem_raw, gap_size, E820_RAM);

    if last_addr > first_addr_past_32bits {
        let high_mem_size = last_addr.unchecked_offset_from(first_addr_past_32bits) + 1;
        add_e820_entry(
            &mut params,
            first_addr_past_32bits.raw_value(),
            high_mem_size,
            E820_RAM,
        );
    }

    if last_addr < end_32bit_gap_start {
        let extended_size = last_addr.unchecked_offset_from(himem_start) as u64 + 1;
        params.e820_table[params.e820_entries as usize - 1].size = extended_size;
    }

    LinuxBootConfigurator::write_bootparams(
        &BootParams::new(&params, GuestAddress(ZERO_PAGE_START)),
        guest_mem,
    )
    .unwrap();
}

pub const fn seg_with_st(selector_index: u16, type_: u8) -> kvm_segment {
    kvm_segment {
        base: 0,
        limit: 0x000f_ffff,
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

pub const fn to_gdt_entry(seg: &kvm_segment) -> u64 {
    let base = seg.base;
    let limit = seg.limit as u64;

    let limit_high = (limit >> 16) & 0xf;
    let flag_bits = ((seg.g as u64) << 3)
        | ((seg.db as u64) << 2)
        | ((seg.l as u64) << 1)
        | (seg.avl as u64);
    let upper_byte = (limit_high << 8) | flag_bits;

    let access_byte = ((seg.present as u64) << 7)
        | ((seg.dpl as u64) << 5)
        | ((seg.s as u64) << 4)
        | (seg.type_ as u64);

    ((base & 0xff00_0000u64) << 32)
        | ((base & 0x00ff_ffffu64) << 16)
        | (limit & 0xffff)
        | (access_byte << 40)
        | (upper_byte << 48)
}

pub fn add_e820_entry(params: &mut boot_params, addr: u64, size: u64, mem_type: u32) {
    if params.e820_entries >= params.e820_table.len() as u8 {
        panic!();
    }
    let entry = &mut params.e820_table[params.e820_entries as usize];
    entry.addr = addr;
    entry.size = size;
    entry.type_ = mem_type;
    params.e820_entries += 1;
}
