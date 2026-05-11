use kvm_bindings::kvm_segment;
use linux_loader::bootparam::boot_params;
use minivmctl::boot::{
    BOOT_CMD, BOOT_CMD_START, BOOT_GDT_OFFSET, E820_RAM, EBDA_START, HIMEM_START,
    KERNEL_BOOT_FLAG_MAGIC_NUMBER, KERNEL_HDR_MAGIC_NUMBER, KERNEL_MIN_ALIGNMENT_BYTES,
    KERNEL_TYPE_OF_LOADER, MEMORY_SIZE, ZERO_PAGE_START, add_e820_entry, load_boot_cmdline,
    seg_with_st, to_gdt_entry, write_boot_params, write_gdt, write_page_tables,
};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

fn guest_memory() -> GuestMemoryMmap<()> {
    GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), MEMORY_SIZE)]).unwrap()
}

fn e820_entry(params: &boot_params, idx: usize) -> (u64, u64, u32) {
    unsafe {
        let entry: *const linux_loader::bootparam::boot_e820_entry = &params.e820_table[idx];
        (
            std::ptr::addr_of!((*entry).addr).read_unaligned(),
            std::ptr::addr_of!((*entry).size).read_unaligned(),
            std::ptr::addr_of!((*entry).type_).read_unaligned(),
        )
    }
}

fn hdr_field<T: Copy>(field: *const T) -> T {
    unsafe { std::ptr::read_unaligned(field) }
}

// Segment helper sets flat code/data bits.
#[test]
fn seg_with_st_sets_expected_fields() {
    let seg = seg_with_st(2, 0b0011);

    assert_eq!(seg.selector, 16);
    assert_eq!(seg.type_, 0b0011);
    assert_eq!(seg.present, 1);
    assert_eq!(seg.s, 1);
    assert_eq!(seg.l, 1);
    assert_eq!(seg.g, 1);
}

// GDT entry encodes nonzero layout.
#[test]
fn to_gdt_entry_encodes_base_limit_and_flags() {
    let seg = kvm_segment {
        base: 0x1234_5678,
        limit: 0x000f_ffff,
        selector: 0,
        type_: 0b1011,
        present: 1,
        dpl: 0,
        db: 0,
        s: 1,
        l: 1,
        g: 1,
        avl: 0,
        unusable: 0,
        padding: 0,
    };

    assert_ne!(to_gdt_entry(&seg), 0);
}

// E820 entry appends once.
#[test]
fn add_e820_entry_appends_entry() {
    let mut params = boot_params::default();

    add_e820_entry(&mut params, 0x1000, 0x2000, 7);

    assert_eq!(params.e820_entries, 1);
    assert_eq!(e820_entry(&params, 0), (0x1000, 0x2000, 7));
}

// Page tables point at next level.
#[test]
fn write_page_tables_sets_expected_entries() {
    let mem = guest_memory();

    write_page_tables(
        &mem,
        GuestAddress(0xb000),
        GuestAddress(0xc000),
        GuestAddress(0xc000),
        GuestAddress(0xa000),
    );

    let pml4: u64 = mem.read_obj(GuestAddress(0xa000)).unwrap();
    let pdpte: u64 = mem.read_obj(GuestAddress(0xb000)).unwrap();
    let pde0: u64 = mem.read_obj(GuestAddress(0xc000)).unwrap();

    assert_eq!(pml4, 0xb001);
    assert_eq!(pdpte, 0xc003);
    assert_eq!(pde0, 0x83);
}

// GDT written to guest memory.
#[test]
fn write_gdt_writes_expected_table() {
    let mem = guest_memory();
    let mut sregs = kvm_bindings::kvm_sregs::default();
    let code = seg_with_st(1, 0b1011);
    let data = seg_with_st(2, 0b0011);

    write_gdt(&mem, &mut sregs, code, data);

    let null: u64 = mem.read_obj(GuestAddress(BOOT_GDT_OFFSET)).unwrap();
    let code_entry: u64 = mem.read_obj(GuestAddress(BOOT_GDT_OFFSET + 8)).unwrap();
    let data_entry: u64 = mem.read_obj(GuestAddress(BOOT_GDT_OFFSET + 16)).unwrap();

    assert_eq!(null, 0);
    assert_eq!(sregs.gdt.base, BOOT_GDT_OFFSET);
    assert_eq!(sregs.gdt.limit, 23);
    assert_ne!(code_entry, 0);
    assert_ne!(data_entry, 0);
}

// Cmdline copied to boot area.
#[test]
fn load_boot_cmdline_writes_command_line() {
    let mem = guest_memory();

    load_boot_cmdline(&mem);

    let mut buf = vec![0u8; BOOT_CMD.len() + 1];
    mem.read_slice(&mut buf, GuestAddress(BOOT_CMD_START))
        .unwrap();
    assert_eq!(&buf[..BOOT_CMD.len()], BOOT_CMD.as_bytes());
}

// Boot params get header and RAM map.
#[test]
fn write_boot_params_sets_linux_header_and_memory_map() {
    let mem = guest_memory();

    load_boot_cmdline(&mem);
    write_boot_params(&mem);

    let params: boot_params = mem.read_obj(GuestAddress(ZERO_PAGE_START)).unwrap();

    let type_of_loader = hdr_field(std::ptr::addr_of!(params.hdr.type_of_loader));
    let boot_flag = hdr_field(std::ptr::addr_of!(params.hdr.boot_flag));
    let header = hdr_field(std::ptr::addr_of!(params.hdr.header));
    let cmd_line_ptr = hdr_field(std::ptr::addr_of!(params.hdr.cmd_line_ptr));
    let cmdline_size = hdr_field(std::ptr::addr_of!(params.hdr.cmdline_size));
    let kernel_alignment = hdr_field(std::ptr::addr_of!(params.hdr.kernel_alignment));

    assert_eq!(type_of_loader, KERNEL_TYPE_OF_LOADER);
    assert_eq!(boot_flag, KERNEL_BOOT_FLAG_MAGIC_NUMBER);
    assert_eq!(header, KERNEL_HDR_MAGIC_NUMBER);
    assert_eq!(cmd_line_ptr, BOOT_CMD_START as u32);
    assert_eq!(cmdline_size, 1 + BOOT_CMD.len() as u32);
    assert_eq!(kernel_alignment, KERNEL_MIN_ALIGNMENT_BYTES);
    assert_eq!(params.e820_entries, 2);
    assert_eq!(e820_entry(&params, 0), (0, EBDA_START, E820_RAM));
    assert_eq!(
        e820_entry(&params, 1),
        (HIMEM_START, MEMORY_SIZE as u64 - HIMEM_START, E820_RAM)
    );
}
