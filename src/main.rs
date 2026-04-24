use kvm_bindings::{
    kvm_segment, kvm_userspace_memory_region, KVM_MAX_CPUID_ENTRIES, KVM_MEM_LOG_DIRTY_PAGES,
};
use kvm_ioctls::Kvm;
use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

const MEMORY_SIZE: usize = 0x30000;
const BOOT_GDT_OFFSET: u64 = 0x500;
const X86_CR0_PG: u64 = 0x80000000;
const X86_CR4_PAE: u64 = 0x20;
const X86_CR0_PE: u64 = 0x1;
const EFER_LME: u64 = 0x100;
const EFER_LMA: u64 = 0x400;

fn main() {
    let boot_pdpte_addr = GuestAddress(0xa000);
    let boot_pde_addr = GuestAddress(0xb000);
    let boot_pte_addr = GuestAddress(0xc000);

    // Memory Initalization

    let kvm = Kvm::new().expect("Open kvm device failed");
    let vm = kvm.create_vm().expect("create vm failed");

    let guest_addr = GuestAddress(0x0);
    let guest_mem = GuestMemoryMmap::<()>::from_ranges(&[(guest_addr, MEMORY_SIZE)]).unwrap();

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

    guest_mem
        .write_slice(
            &(boot_pde_addr.raw_value() as u64 | 1).to_le_bytes(),
            boot_pdpte_addr,
        )
        .unwrap();

    guest_mem
        .write_slice(
            &(boot_pte_addr.raw_value() as u64 | 0b11).to_le_bytes(),
            boot_pde_addr,
        )
        .unwrap();

    for i in 0..512 {
        guest_mem
            .write_slice(
                &((i << 12) + 0b11u64).to_le_bytes(),
                boot_pte_addr.unchecked_add(i * 8),
            )
            .unwrap()
    }
    // CPU initalization

    let mut vcpu = vm.create_vcpu(0).expect("Failed creating vcpu");
    let kvm_cpuid = kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES).unwrap();

    let mut regs = vcpu.get_regs().unwrap();
    regs.rip = 0;
    regs.rflags = 2;
    vcpu.set_regs(&regs).unwrap();

    //sregs
    const CODE_SEG: kvm_segment = seg_with_st(1, 0b1011);
    const DATA_SEG: kvm_segment = seg_with_st(2, 0b0011);

    let mut sregs = vcpu.get_sregs().unwrap();

    // sregs.cs.selector = 0;
    // sregs.cs.base = 0;

    sregs.cs = CODE_SEG;
    sregs.ds = DATA_SEG;
    sregs.es = DATA_SEG;
    sregs.fs = DATA_SEG;
    sregs.gs = DATA_SEG;
    sregs.ss = DATA_SEG;

    sregs.cr3 = boot_pdpte_addr.raw_value() as u64;
    sregs.cr4 |= X86_CR4_PAE;
    sregs.cr0 |= X86_CR0_PG;
    sregs.efer |= EFER_LMA | EFER_LME; //??

    // construct gdt table
    let gdt_table: [u64; 3] = [0, to_gdt_entry(&CODE_SEG), to_gdt_entry(&DATA_SEG)];
    let boot_gdt_address = GuestAddress(BOOT_GDT_OFFSET); //??

    for (index, entry) in gdt_table.iter().enumerate() {
        let addr = guest_mem
            .checked_offset(boot_gdt_address, index * std::mem::size_of::<u64>())
            .unwrap();
        guest_mem.write_obj(*entry, addr).unwrap();
    }

    sregs.gdt.base = BOOT_GDT_OFFSET;
    sregs.gdt.limit = std::mem::size_of_val(&gdt_table) as u16 - 1;

    vcpu.set_sregs(&sregs).unwrap();
    // execute sth

    // let code = [0xb8, 0x42, 0x00, 0x3e, 0xa3, 0x00, 0x10, 0xf4];
    let code = [
        0xb8, 0x42, 0x00, 0x00, 0x00, 0x3e, 0xa3, 0x00, 0x00, 0x01, 0x00, 0xf4,
    ];
    guest_mem.write_slice(&code, GuestAddress(0x0)).unwrap();
    let reason = vcpu.run().unwrap();
    println!("exit reason: {:?}", reason);
    let regs = vcpu.get_regs().unwrap();
    println!("rax: {:x}, rip: {:X?}", regs.rax, regs.rip);
    println!(
        "memory at 0x10000: 0x{:X}",
        guest_mem.read_obj::<u16>(GuestAddress(0x1000)).unwrap()
    );
}

const fn seg_with_st(selector_index: u16, type_: u8) -> kvm_segment {
    kvm_segment {
        base: 0,
        limit: 0x000fffff,
        selector: selector_index << 3,
        type_,
        present: 1,
        dpl: 0,
        db: 1,
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
