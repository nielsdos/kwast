use multiboot2::{BootInformation, MemoryMapTag};
use spin::Mutex;

use crate::arch::address::{PhysAddr, VirtAddr};
use crate::arch::paging::{ActiveMapping, CacheType, EntryFlags};

/// Map result.
pub type MappingResult = Result<(), MappingError>;

/// Error during mapping request.
#[derive(Debug)]
pub enum MappingError {
    /// Out of memory.
    OOM
}

/// The default frame allocator.
///
/// How does this allocator work?
/// Instead of having a fixed area in the memory to keep the stack,
/// we let each free frame contain a pointer to the next free frame on the stack.
/// This limits the amount of virtual memory we need to reserve.
///
/// When we allocate a frame, we map it to the virtual memory and read the pointer.
/// Then we move the head. There is no unnecessary mapping happening here.
///
/// It is likely that, for an allocation, the data will be accessed anyway after the mapping.
/// For a free, it is likely that the data was already accessed.
#[derive(Debug)]
pub struct FrameAllocator {
    reserved_end: PhysAddr,
    top: PhysAddr,
}

impl FrameAllocator {
    /// Initializes the allocator.
    fn init(&mut self, mboot_struct: &BootInformation, reserved_end: PhysAddr) {
        self.reserved_end = reserved_end.align_up();

        self.apply_mmap(
            mboot_struct.memory_map_tag().expect("Memory map is required")
        );
    }

    /// Empty, uninitialized allocator.
    const fn empty() -> Self {
        FrameAllocator {
            reserved_end: PhysAddr::null(),
            top: PhysAddr::null(),
        }
    }

    /// Applies the memory map.
    fn apply_mmap(&mut self, tag: &MemoryMapTag) {
        // TODO: split to arch partially?

        let mut mapping = unsafe { ActiveMapping::new() };

        // Will be the last entry of the PML2 (PML2 exists)
        let tmp_2m_map_addr = VirtAddr::new(511 * 0x200000);
        // PML1 exists for the corresponding PML2
        let tmp_4k_map_addr = VirtAddr::new(0x1000);

        // Previous entry address
        let mut top: usize = 0;
        let mut prev_entry_addr: *mut usize = &mut top as *mut _;

        let mut fill_list_entry = |paddr: usize, vaddr: VirtAddr, count: u16, debug: bool| {
            let mut paddr = paddr;
            let mut count = count;

            while count != 0 {
                if debug { println!("{:p}", prev_entry_addr); }
                unsafe { prev_entry_addr.write_volatile(paddr); }

                prev_entry_addr = paddr as *mut _;

                count -= 1;
                paddr += 0x1000;
            }
        };

        for x in tag.memory_areas() {
            // There is actually no guarantee about the sanitization of the data.
            // While it is rare that the addresses won't be page aligned, there's apparently been
            // cases before, where it wasn't page aligned.
            let mut start = PhysAddr::new(x.start_address() as usize).align_up();
            let end = PhysAddr::new(x.end_address() as usize).align_down();

            // Adjust for reserved area
            if start < self.reserved_end {
                start = self.reserved_end;
                if start > end {
                    continue;
                }
            }

            let mut current = start.as_usize();
            let end = end.as_usize();

            // Sets the first available address ( = top of the stack).
            if unlikely!(top == 0) {
                top = current;
            }

            // TODO: explain how this works & why 2M mapping

            // Process 4K parts at beginning until we have 2M parts.
            while current < end && (current & 0x1fffff) != 0 {
                unsafe {
                    prev_entry_addr.write_volatile(current);
                }

                mapping.map_4k(tmp_4k_map_addr, PhysAddr::new(current), EntryFlags::PRESENT | EntryFlags::WRITABLE, CacheType::WriteBack)
                    .expect("failed to map");

                prev_entry_addr = tmp_4k_map_addr.as_usize() as *mut _;

                // fill_list_entry(current, tmp_4k_map_addr, 1, false);

                current += 0x1000;
            }
            /*
                        // Process 2M parts until a 2M part doesn't fit anymore.
                        while current + 0x200000 < end {
                            mapping.map_2m(tmp_2m_map_addr, PhysAddr::new(current), EntryFlags::PRESENT | EntryFlags::WRITABLE)
                                .expect("failed to map");

                            println!("2M fill");
                            fill_list_entry(current, tmp_2m_map_addr, 0x200, true);

                            current += 0x200000;
                        }
            */
            /*
                        // Process 4K parts at end.
                        while current < end {
                            mapping.map_4k(tmp_4k_map_addr, PhysAddr::new(current), EntryFlags::PRESENT | EntryFlags::WRITABLE)
                                .expect("failed to map");

                            fill_list_entry(current, 1);

                            current += 0x1000;
                        }*/
        }

        // End
        unsafe {
            prev_entry_addr.write_volatile(0);
        }
        self.top = PhysAddr::new(top);

        // TODO: unmap
    }

    /// Moves the top of the stack.
    fn move_top(&mut self, vaddr: VirtAddr) {
        // Read and set the next top address.
        let ptr = vaddr.as_usize() as *mut usize;
        self.top = PhysAddr::new(unsafe { *ptr });
    }

    /// Gets a page and maps it to a virtual address.
    pub fn map_page(&mut self, vaddr: VirtAddr, flags: EntryFlags, cache_type: CacheType) -> MappingResult {
        if unlikely!(self.top.is_null()) {
            return Err(MappingError::OOM);
        }

        // Maps the page to the destination virtual address, then moves the top.
        let mut mapping = unsafe { ActiveMapping::new() };
        mapping.map_4k(vaddr, self.top, flags, cache_type)?;
        self.move_top(vaddr);

        Ok(())
    }

    /// Consumes the top and moves it. This function is used internally for memory management.
    /// It allows the paging component to get the top directly and let it move.
    /// This is faster than going via `map_page`.
    pub fn consume_and_move_top<F>(&mut self, f: F) -> MappingResult
        where F: FnOnce(PhysAddr) -> VirtAddr {
        if unlikely!(self.top.is_null()) {
            return Err(MappingError::OOM);
        }

        self.move_top(f(self.top));

        Ok(())
    }
}

/// The default frame allocator instance.
static ALLOCATOR: Mutex<FrameAllocator> = Mutex::new(FrameAllocator::empty());

/// Inits the physical frame allocator.
pub fn init(mboot_struct: &BootInformation, reserved_end: usize) {
    ALLOCATOR.lock().init(mboot_struct, PhysAddr::new(reserved_end));
}

/// Maps a page.
pub fn map_page(vaddr: VirtAddr, flags: EntryFlags, cache_type: CacheType) -> MappingResult {
    let mut mapping = unsafe { ActiveMapping::new() };

    // Pre-allocating the required tables.
    // This can be done without locking the PMM the whole time, which prevents a deadlock.
    mapping.ensure_4k_tables_exist(vaddr)?;

    ALLOCATOR.lock().map_page(vaddr, flags, cache_type)
}

/// Consumes the top and then lets it move. (internal memory management use only)
/// See docs at impl.
#[inline]
pub unsafe fn consume_and_move_top<F>(f: F) -> MappingResult
    where F: FnOnce(PhysAddr) -> VirtAddr {
    ALLOCATOR.lock().consume_and_move_top(f)
}
