use core::mem::size_of;

use crate::arch::address::VirtAddr;
use crate::arch::paging::{EntryFlags, PAGE_SIZE};
use crate::arch::simd::SimdState;
use crate::arch::{preempt_disable, preempt_enable};
use crate::mm::mapper::MemoryError;
use crate::mm::vma_allocator::{LazilyMappedVma, MappableVma, MappedVma};
use crate::sync::spinlock::{PreemptCounterInfluence, RwLock, Spinlock};
use crate::tasking::file::FileDescriptorTable;
use crate::tasking::protection_domain::ProtectionDomain;
use crate::tasking::scheme::ReplyPayloadTcb;
use crate::tasking::scheme_container::{schemes, SchemeId};
use crate::wasm::vmctx::{VmContextContainer, WASM_PAGE_SIZE};
use alloc::boxed::Box;
use alloc::sync::Arc;
use atomic::Atomic;
use core::borrow::Borrow;
use core::cmp::Ordering;
use spin::MutexGuard;

/// Stack size in bytes.
const STACK_SIZE: usize = 1024 * 256;

/// Amount of guard pages for stack underflow.
const AMOUNT_GUARD_PAGES: usize = 2;

/// The stack of a thread.
pub struct Stack {
    vma: MappedVma,
    current_location: Atomic<VirtAddr>,
}

#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Hash)]
#[repr(transparent)]
pub struct ThreadId(u64);

const_assert!(Atomic::<ThreadId>::is_lock_free());

impl ThreadId {
    /// Create new thread id.
    pub fn new() -> Self {
        use core::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(NEXT.fetch_add(1, Ordering::SeqCst))
    }

    /// Thread id 0, useful for markers / sentinels if you know that thread id 0 won't be used.
    pub const fn zero() -> Self {
        Self(0)
    }
}

impl Default for ThreadId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
#[repr(C, align(8))]
pub enum ThreadStatus {
    Runnable,
    Blocked,
    Exit(u32),
}

const_assert!(Atomic::<ThreadStatus>::is_lock_free());

struct StaticWasmThreadData {
    code: MappedVma,
    _vmctx_container: VmContextContainer,
}

pub struct Thread {
    pub stack: Stack,
    id: ThreadId,
    heap: RwLock<LazilyMappedVma>,
    static_wasm_data: Spinlock<Option<StaticWasmThreadData>>,
    simd_state: SimdState,
    domain: ProtectionDomain,
    status: Atomic<ThreadStatus>,
    file_descriptor_table: Spinlock<FileDescriptorTable>, // TODO: avoid locks if we're the only owner
    pub reply: ReplyPayloadTcb,
    /// On what scheme are we blocked on? Only applicable for sync IPC.
    /// If this is equal to the sentinel value, we aren't blocked on a scheme.
    blocked_on: Atomic<SchemeId>,
}

impl Thread {
    /// Creates a thread.
    /// Unsafe because it's possible to set an entry point.
    pub unsafe fn create(
        domain: ProtectionDomain,
        entry: VirtAddr,
        first_arg: usize,
    ) -> Result<Thread, MemoryError> {
        // TODO: lazily allocate in the future?
        let stack_guard_size: usize = AMOUNT_GUARD_PAGES * PAGE_SIZE;
        let stack = {
            preempt_disable();
            let guard = domain.temporarily_switch();
            let mut stack = Stack::create(&domain, STACK_SIZE, stack_guard_size)?;
            stack.prepare_trampoline(entry, first_arg);
            drop(guard);
            preempt_enable();
            stack
        };
        Ok(Self::new(stack, domain))
    }

    /// Creates a new thread from given parameters.
    pub fn new(stack: Stack, domain: ProtectionDomain) -> Self {
        // TODO
        let mut fdt = FileDescriptorTable::new();
        fdt.insert_lowest({
            let mut tmp = schemes()
                .read()
                .open_self(Box::new([]))
                .expect("self scheme");
            tmp.set_pre_open_path(Box::new(*b"."));
            tmp
        });

        Self {
            stack,
            heap: RwLock::new(LazilyMappedVma::dummy()),
            id: ThreadId::new(),
            static_wasm_data: Spinlock::new(None),
            domain,
            simd_state: SimdState::new(),
            status: Atomic::new(ThreadStatus::Runnable),
            file_descriptor_table: Spinlock::new(fdt),
            reply: ReplyPayloadTcb::new(),
            blocked_on: Atomic::new(SchemeId::sentinel()),
        }
    }

    /// Sets the thread wasm data.
    /// Unsafe when incorrect data is passed, or when used data is overwritten.
    pub unsafe fn set_wasm_data(
        &self,
        code_vma: MappedVma,
        heap_vma: LazilyMappedVma,
        vmctx_container: VmContextContainer,
    ) {
        *self.heap.write() = heap_vma;
        *self.static_wasm_data.lock() = Some(StaticWasmThreadData {
            code: code_vma,
            _vmctx_container: vmctx_container,
        });
    }

    /// Gets the file descriptor table.
    #[inline]
    pub fn file_descriptor_table(
        &self,
    ) -> MutexGuard<FileDescriptorTable, PreemptCounterInfluence> {
        self.file_descriptor_table.lock()
    }

    /// Gets the thread id.
    #[inline]
    pub fn id(&self) -> ThreadId {
        self.id
    }

    /// Gets the current allocated heap size in WebAssembly pages.
    pub fn heap_size(&self) -> usize {
        self.heap.read().size()
    }

    /// Grows the heap by `wasm_pages` WebAssembly pages.
    pub fn heap_grow(&self, wasm_pages: u32) -> u32 {
        self.heap
            .write()
            .expand((wasm_pages as usize) * WASM_PAGE_SIZE)
            .map_or(core::u32::MAX, |x| (x / WASM_PAGE_SIZE) as u32)
    }

    /// Unmaps the memory that this thread holds.
    /// Unsafe because you can totally break memory mappings and safety if you call this
    /// while memory of this thread is still used somewhere.
    pub unsafe fn unmap_memory(&self) {
        self.domain.with(|vma, mapping| {
            if let Some(ref mut data) = *self.static_wasm_data.lock() {
                vma.destroy_vma(mapping, &data.code);
            }
            vma.destroy_vma(mapping, &self.stack.vma);
            let mut heap = self.heap.write();
            vma.destroy_vma(mapping, &*heap);
            *heap = LazilyMappedVma::dummy();
        });
    }

    /// Gets the current protection domain.
    #[inline]
    pub fn domain(&self) -> &ProtectionDomain {
        &self.domain
    }

    /// Handle a page fault for this thread. Returns true if handled successfully.
    #[inline]
    pub fn page_fault(&self, fault_addr: VirtAddr) -> bool {
        self.domain
            .with(|_vma, mapping| self.heap.write().try_handle_page_fault(mapping, fault_addr))
    }

    /// Save SIMD state.
    #[inline]
    pub fn save_simd(&self) {
        self.simd_state.save();
    }

    /// Restore SIMD state.
    #[inline]
    pub fn restore_simd(&self) {
        self.simd_state.restore();
    }

    /// Sets the status.
    #[inline]
    pub fn set_status(&self, new_status: ThreadStatus) {
        self.status.store(new_status, atomic::Ordering::Release);
    }

    /// Compare exchange status.
    #[inline]
    pub fn status_compare_exchange(
        &self,
        current: ThreadStatus,
        new: ThreadStatus,
        success: atomic::Ordering,
        failure: atomic::Ordering,
    ) -> Result<ThreadStatus, ThreadStatus> {
        self.status.compare_exchange(current, new, success, failure)
    }

    /// Gets the status.
    #[inline]
    pub fn status(&self) -> ThreadStatus {
        self.status.load(atomic::Ordering::Acquire)
    }

    /// Sets blocked on.
    #[inline]
    pub fn set_blocked_on(&self, blocked_on: SchemeId) {
        self.blocked_on.store(blocked_on, atomic::Ordering::Release);
    }

    /// Sets blocked on.
    #[inline]
    pub fn blocked_on(&self) -> SchemeId {
        self.blocked_on.load(atomic::Ordering::Acquire)
    }
}

impl PartialEq for Thread {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for Thread {}

impl PartialOrd for Thread {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.id.partial_cmp(&other.id)
    }
}

impl Ord for Thread {
    fn cmp(&self, other: &Self) -> Ordering {
        self.id.cmp(&other.id)
    }
}

impl Borrow<ThreadId> for Arc<Thread> {
    fn borrow(&self) -> &ThreadId {
        &self.id
    }
}

impl Stack {
    /// Creates a stack.
    pub fn create(
        domain: &ProtectionDomain,
        size: usize,
        guard_size: usize,
    ) -> Result<Stack, MemoryError> {
        let vma = {
            let flags = EntryFlags::PRESENT | EntryFlags::WRITABLE | EntryFlags::NX;

            domain.with(|vma, mapping| {
                vma.create_vma(size + guard_size)?
                    .map(mapping, guard_size, size, flags)
            })?
        };
        Ok(Stack::new(vma))
    }

    /// Creates a new stack from given parameters.
    pub fn new(vma: MappedVma) -> Self {
        let current_location = vma.address() + vma.size();
        Self {
            vma,
            current_location: Atomic::new(current_location),
        }
    }

    /// Gets the current location.
    #[inline]
    pub fn get_current_location(&self) -> VirtAddr {
        self.current_location.load(atomic::Ordering::Acquire)
    }

    /// Sets the current location.
    #[inline]
    pub fn set_current_location(&self, location: VirtAddr) {
        //debug_assert!(
        //    self.vma.get().is_dummy() || self.vma.get().is_contained(location),
        //    "the address {:?} does not belong to the thread's stack",
        //    location,
        //);
        self.current_location
            .store(location, atomic::Ordering::Release);
    }

    /// Pushes a value on the stack.
    /// Unsafety: might go out of bounds, might push invalid value, might data race.
    /// Data race can be prevented if the stack is not shared yet (e.g. on trampoline setup).
    pub unsafe fn push<T>(&mut self, value: T) {
        let mut current = self.get_current_location();
        current -= size_of::<T>();
        let ptr = current.as_mut();
        *ptr = value;
        self.set_current_location(current);
    }
}
