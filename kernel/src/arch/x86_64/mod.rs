use core::cmp::max;

use crate::arch::address::VirtAddr;
use crate::arch::x86_64::address::PhysAddr;
use crate::arch::x86_64::paging::{ActiveMapping, EntryFlags};
use crate::mm;
use crate::mm::mapper::MemoryMapper;
use multiboot2::ElfSectionFlags;

#[macro_use]
pub mod macros;
pub mod address;
pub mod interrupts;
pub mod paging;
pub mod port;
pub mod vga_text;

// For tests
pub mod qemu;
pub mod serial;

extern "C" {
    static KERNEL_END_PTR: usize;
}

/// Initializes arch-specific stuff.
#[no_mangle]
pub extern "C" fn entry(mboot_addr: usize) {
    interrupts::init();

    let kernel_end = unsafe { &KERNEL_END_PTR as *const _ as usize };
    let mboot_struct = unsafe { multiboot2::load(mboot_addr) };
    let mboot_end = mboot_struct.end_address();
    let reserved_end = max(kernel_end, mboot_end);

    // Map sections correctly
    {
        let mut mapping = ActiveMapping::get();
        let sections = mboot_struct
            .elf_sections_tag()
            .expect("no elf sections tag");
        for x in sections.sections() {
            if x.flags().is_empty()
                || x.flags() == ElfSectionFlags::WRITABLE | ElfSectionFlags::ALLOCATED
            {
                continue;
            }

            let mut paging_flags: EntryFlags = EntryFlags::PRESENT;

            if x.flags().contains(ElfSectionFlags::WRITABLE) {
                paging_flags |= EntryFlags::WRITABLE;
            }

            if !x.flags().contains(ElfSectionFlags::EXECUTABLE) {
                paging_flags |= EntryFlags::NX;
            }

            //println!("{:#x}-{:#x} {:?}", x.start_address(), x.end_address(), x.flags());

            let start = VirtAddr::new(x.start_address() as usize).align_down();
            mapping
                .map_range_physical(
                    start,
                    PhysAddr::new(start.as_usize()),
                    (x.end_address() - start.as_u64()) as usize, // No need for page alignment of size
                    paging_flags,
                )
                .unwrap();
        }
    }

    mm::pmm::get().init(&mboot_struct, reserved_end);

    let reserved_end = VirtAddr::new(reserved_end).align_up();
    crate::kernel_run(reserved_end);
}

/// Halt instruction. Waits for interrupt.
pub fn halt() {
    unsafe {
        asm!("hlt" :::: "volatile");
    }
}
