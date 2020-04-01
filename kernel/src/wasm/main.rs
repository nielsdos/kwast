//! Based on https://github.com/bytecodealliance/wasmtime/tree/master/crates/jit/src

use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::{CodegenError, Context};
use cranelift_wasm::{translate_module, Global, Memory, MemoryIndex};
use cranelift_wasm::{FuncIndex, FuncTranslator, WasmError};

use crate::arch::address::{align_up, VirtAddr};
use crate::arch::paging::{ActiveMapping, EntryFlags};
use crate::mm::mapper::{MemoryError, MemoryMapper};
use crate::mm::vma_allocator::{LazilyMappedVma, MappableVma, MappedVma, Vma};
use crate::tasking::scheduler;
use crate::tasking::scheduler::{add_and_schedule_thread, with_core_scheduler, SwitchReason};
use crate::tasking::thread::Thread;
use crate::wasm::func_env::FuncEnv;
use crate::wasm::module_env::{
    DataInitializer, Export, FunctionBody, FunctionImport, ModuleEnv, TableElements,
};
use crate::wasm::reloc_sink::{RelocSink, RelocationTarget};
use crate::wasm::runtime::{RUNTIME_MEMORY_GROW_IDX, RUNTIME_MEMORY_SIZE_IDX};
use crate::wasm::table::Table;
use crate::wasm::vmctx::{
    VmContext, VmContextContainer, VmFunctionImportEntry, VmTableElement, HEAP_GUARD_SIZE,
    HEAP_SIZE, WASM_PAGE_SIZE,
};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ptr::{copy_nonoverlapping, write_unaligned};
use cranelift_codegen::binemit::{NullStackmapSink, NullTrapSink, Reloc};
use cranelift_codegen::isa::TargetIsa;

// TODO: in some areas, a bump allocator could be used to quickly allocate some vectors.

// TODO: move me
fn runtime_memory_size(_vmctx: &VmContext, idx: MemoryIndex) -> u32 {
    assert_eq!(idx.as_u32(), 0);
    let heap_size = with_core_scheduler(|s| s.get_current_thread().heap_size());
    (heap_size / WASM_PAGE_SIZE) as u32
}

// TODO: move me
fn runtime_memory_grow(_vmctx: &VmContext, idx: MemoryIndex, wasm_pages: u32) -> u32 {
    assert_eq!(idx.as_u32(), 0);
    with_core_scheduler(|s| s.get_current_thread().heap_grow(wasm_pages))
}

fn wasi_environ_sizes_get(vmctx: &VmContext, environ_count_ptr: u32, environ_size_ptr: u32) -> u16 {
    println!(
        "environ_sizes_get {:#x} {:#x}",
        environ_count_ptr, environ_size_ptr
    );

    // TODO: make a convenient method for this
    let environ_count_ptr: *mut u32 = (vmctx.heap_ptr + environ_count_ptr as usize).as_mut();
    let environ_size_ptr: *mut u32 = (vmctx.heap_ptr + environ_size_ptr as usize).as_mut();

    unsafe {
        // TODO: dummy values atm
        *environ_count_ptr = 0;
        *environ_size_ptr = 4;
    }

    println!("hi");

    // TODO
    0
}

fn wasi_environ_get(_vmctx: &VmContext, environ_ptr: u32, environ_buf: u32) -> u16 {
    // TODO
    println!("wasi_environ_get {} {}", environ_ptr, environ_buf);
    0
}

fn wasi_fd_write(_vmctx: &VmContext, fd: u32, iovs_ptr: u32, iovs_len: u32, nwritten: u32) -> u16 {
    // TODO
    println!("fd_write {} {} {} {}", fd, iovs_ptr, iovs_len, nwritten);
    0
}

fn wasi_proc_exit(_vmctx: &VmContext, exitcode: u32) -> ! {
    println!("proc_exit: {}", exitcode);
    scheduler::switch_to_next(SwitchReason::Exit);
    unreachable!()
}

#[derive(Debug)]
pub enum Error {
    /// WebAssembly translation error.
    WasmError(WasmError),
    /// Code generation error.
    CodegenError(CodegenError),
    /// Memory error.
    MemoryError(MemoryError),
    /// No start specified.
    NoStart,
}

struct CompileResult<'data> {
    isa: Box<dyn TargetIsa>,
    contexts: Box<[Context]>,
    start_func: Option<FuncIndex>,
    memories: Box<[Memory]>,
    data_initializers: Box<[DataInitializer<'data>]>,
    function_imports: Box<[FunctionImport]>,
    tables: Box<[cranelift_wasm::Table]>,
    table_elements: Box<[TableElements]>,
    globals: Box<[Global]>,
    total_size: usize,
}

struct Instantiation<'r, 'data> {
    compile_result: &'r CompileResult<'data>,
    func_offsets: Vec<usize>,
}

impl<'data> CompileResult<'data> {
    /// Compile result to instantiation.
    pub fn instantiate(&self) -> Instantiation {
        Instantiation::new(self)
    }
}

impl<'r, 'data> Instantiation<'r, 'data> {
    /// Creates a new instantiation.
    fn new(compile_result: &'r CompileResult<'data>) -> Self {
        let capacity = compile_result.contexts.len();

        Self {
            compile_result,
            func_offsets: Vec::with_capacity(capacity),
        }
    }

    /// Gets the offset of the defined functions in the function array.
    fn defined_function_offset(&self) -> usize {
        self.compile_result.function_imports.len()
    }

    // Helper to get  the function address from a function index.
    fn get_func_address(&self, code_vma: &MappedVma, index: FuncIndex) -> VirtAddr {
        let offset = self.func_offsets[index.as_u32() as usize - self.defined_function_offset()];
        VirtAddr::new(code_vma.address().as_usize() + offset)
    }

    /// Emit code.
    fn emit(&mut self) -> Result<(MappedVma, LazilyMappedVma, Vec<RelocSink>), Error> {
        // Create code area, will be made executable read-only later.
        let code_vma = {
            let len = align_up(self.compile_result.total_size);
            let flags = EntryFlags::PRESENT | EntryFlags::WRITABLE | EntryFlags::NX;
            Vma::create(len)
                .and_then(|x| x.map(0, len, flags))
                .map_err(Error::MemoryError)?
        };

        let heap_vma = {
            let mem = self.compile_result.memories[0];
            let minimum = mem.minimum as usize * WASM_PAGE_SIZE;

            // TODO: func_env assumes 4GiB is available, also makes it so that we can't construct
            //       a pointer outside (See issue #10 also)
            //let maximum = mem
            //    .maximum
            //    .map_or(HEAP_SIZE, |m| (m as u64) * WASM_PAGE_SIZE as u64);
            let maximum = HEAP_SIZE;

            if minimum as u64 > HEAP_SIZE || maximum > HEAP_SIZE {
                return Err(Error::MemoryError(MemoryError::InvalidRange));
            }

            let len = maximum + HEAP_GUARD_SIZE;
            let flags = EntryFlags::PRESENT | EntryFlags::WRITABLE | EntryFlags::NX;
            Vma::create(len as usize)
                .and_then(|x| Ok(x.map_lazily(minimum, flags)))
                .map_err(Error::MemoryError)?
        };

        // Emit code
        let capacity = self.compile_result.contexts.len();
        let mut reloc_sinks: Vec<RelocSink> = Vec::with_capacity(capacity);
        let mut offset: usize = 0;

        for context in self.compile_result.contexts.iter() {
            let mut reloc_sink = RelocSink::new();
            let mut trap_sink = NullTrapSink {};
            let mut null_stackmap_sink = NullStackmapSink {};

            let info = unsafe {
                let ptr = (code_vma.address() + offset).as_mut();

                context.emit_to_memory(
                    &*self.compile_result.isa,
                    ptr,
                    &mut reloc_sink,
                    &mut trap_sink,
                    &mut null_stackmap_sink,
                )
            };

            self.func_offsets.push(offset);
            reloc_sinks.push(reloc_sink);

            offset += info.total_size as usize;
        }

        Ok((code_vma, heap_vma, reloc_sinks))
    }

    /// Emit and link.
    pub fn emit_and_link(&mut self) -> Result<Thread, Error> {
        let defined_function_offset = self.defined_function_offset();

        let (code_vma, heap_vma, reloc_sinks) = self.emit()?;

        // Relocations
        for (idx, reloc_sink) in reloc_sinks.iter().enumerate() {
            for relocation in &reloc_sink.relocations {
                let reloc_addr = code_vma.address().as_usize()
                    + self.func_offsets[idx]
                    + relocation.code_offset as usize;

                // Determine target address.
                let target_off = match relocation.target {
                    RelocationTarget::UserFunction(target_idx) => {
                        self.func_offsets[target_idx.as_u32() as usize - defined_function_offset]
                    }
                    RelocationTarget::RuntimeFunction(idx) => match idx {
                        RUNTIME_MEMORY_GROW_IDX => runtime_memory_grow as usize,
                        RUNTIME_MEMORY_SIZE_IDX => runtime_memory_size as usize,
                        _ => unreachable!(),
                    },
                    RelocationTarget::LibCall(_libcall) => unimplemented!(),
                    RelocationTarget::JumpTable(jt) => {
                        let ctx = &self.compile_result.contexts[idx];
                        let offset = ctx
                            .func
                            .jt_offsets
                            .get(jt)
                            .expect("jump table should exist");
                        // TODO: is this correct?
                        self.func_offsets[idx - defined_function_offset] + *offset as usize
                    }
                };

                // Relocate!
                match relocation.reloc {
                    Reloc::X86PCRel4 | Reloc::X86CallPCRel4 => {
                        let delta = target_off
                            .wrapping_sub(self.func_offsets[idx] + relocation.code_offset as usize)
                            .wrapping_add(relocation.addend as usize);

                        unsafe {
                            write_unaligned(reloc_addr as *mut u32, delta as u32);
                        }
                    }
                    Reloc::Abs8 => {
                        let delta = target_off.wrapping_add(relocation.addend as usize);

                        unsafe {
                            write_unaligned(reloc_addr as *mut u64, delta as u64);
                        }
                    }
                    Reloc::X86PCRelRodata4 => { /* ignore */ }
                    _ => unimplemented!(),
                }
            }
        }

        // Debug code: print the bytes of the code section.
        // self.print_code_as_hex(&code_vma);

        // Now the code is written, change it to read-only & executable.
        {
            let mut mapping = ActiveMapping::get();
            let flags = EntryFlags::PRESENT;
            mapping
                .change_flags_range(code_vma.address(), code_vma.size(), flags)
                .map_err(Error::MemoryError)?;
        };

        // Determine start function. If it's not given, search for "_start" as specified by WASI.
        let start_func = self.compile_result.start_func.ok_or(Error::NoStart)?;

        let vmctx_container = self.create_vmctx_container(&code_vma, &heap_vma);

        Ok(Thread::create(
            self.get_func_address(&code_vma, start_func),
            code_vma,
            heap_vma,
            vmctx_container,
        )
        .map_err(Error::MemoryError)?)
    }

    /// Print code section as hex.
    #[allow(dead_code)]
    fn print_code_as_hex(&self, code_vma: &MappedVma) {
        for i in 0..self.compile_result.total_size {
            let address = code_vma.address().as_usize() + i;
            unsafe {
                let ptr = address as *const u8;
                print!("{:#x}, ", *ptr);
            }
        }
        println!();
    }

    /// Creates the VmContext container.
    fn create_vmctx_container(
        &self,
        code_vma: &MappedVma,
        heap_vma: &LazilyMappedVma,
    ) -> VmContextContainer {
        // TODO: split this function

        // Create the vm context.
        let mut vmctx_container = {
            // Initialize table vectors.
            let tables: Vec<Table> = self
                .compile_result
                .tables
                .iter()
                .map(|x| Table::new(x))
                .collect();

            unsafe {
                VmContextContainer::new(
                    heap_vma.address(),
                    self.compile_result.globals.len() as u32,
                    self.compile_result.function_imports.len() as u32,
                    tables,
                )
            }
        };

        // Resolve import addresses.
        {
            // Safety: we are the only ones who have access to this slice right now.
            let function_imports = unsafe { vmctx_container.function_imports_as_mut_slice() };

            for (i, import) in self.compile_result.function_imports.iter().enumerate() {
                println!("{} {:?}", i, import);

                // TODO: improve this
                function_imports[i] = match import.module.as_str() {
                    "os" => {
                        // TODO: hardcoded to a fixed function atm
                        VmFunctionImportEntry {
                            address: VirtAddr::new(test_func as usize),
                        }
                    }
                    "wasi_snapshot_preview1" => {
                        // TODO
                        match import.field.as_str() {
                            "environ_sizes_get" => VmFunctionImportEntry {
                                address: VirtAddr::new(wasi_environ_sizes_get as usize),
                            },
                            "fd_write" => VmFunctionImportEntry {
                                address: VirtAddr::new(wasi_fd_write as usize),
                            },
                            "environ_get" => VmFunctionImportEntry {
                                address: VirtAddr::new(wasi_environ_get as usize),
                            },
                            "proc_exit" => VmFunctionImportEntry {
                                address: VirtAddr::new(wasi_proc_exit as usize),
                            },
                            _ => unimplemented!(),
                        }
                    }
                    _ => unimplemented!(),
                };
            }
        }

        // Create tables.
        {
            // Fill in the tables.
            for elements in self.compile_result.table_elements.iter() {
                // TODO: support this and verify bounds?
                assert!(elements.base.is_none(), "not implemented yet");

                let offset = elements.offset;
                let table = vmctx_container.get_table(elements.index);

                for (i, func_idx) in elements.elements.iter().enumerate() {
                    table.set(
                        i + offset,
                        VmTableElement {
                            address: self.get_func_address(code_vma, *func_idx),
                        },
                    );
                }
            }

            // Run data initializers
            {
                // TODO: bounds check? must not go beyond minimum? Otherwise the init would be odd (also "ensure mapped" would be weird)

                for initializer in self.compile_result.data_initializers.iter() {
                    assert_eq!(initializer.memory_index.as_u32(), 0);
                    // TODO: support this
                    assert!(initializer.base.is_none());

                    // TODO: doesn't work because it's not mapped atm
                    //       Solution: "ensure mapped" method

                    let offset = heap_vma.address() + initializer.offset;
                    println!(
                        "Copy {:?} to {:?} length {}",
                        initializer.data.as_ptr(),
                        offset.as_mut::<u8>(),
                        initializer.data.len()
                    );
                    let offset = heap_vma.address() + initializer.offset;
                    unsafe {
                        copy_nonoverlapping(
                            initializer.data.as_ptr(),
                            offset.as_mut::<u8>(),
                            initializer.data.len(),
                        );
                    }
                }
            }

            vmctx_container.write_tables_to_vmctx();
        }

        // Create globals
        {
            for (i, global) in self.compile_result.globals.iter().enumerate() {
                // Safety: valid index
                unsafe {
                    vmctx_container.set_global(i as u32, &global);
                }
            }
        }

        vmctx_container
    }
}

/// Runs WebAssembly from a buffer.
pub fn run(buffer: &[u8]) -> Result<(), Error> {
    let thread = {
        let compile_result = compile(buffer)?;
        let mut instantiation = compile_result.instantiate();
        instantiation.emit_and_link()?
    };

    add_and_schedule_thread(thread);

    Ok(())
}

fn test_func(_vmctx: *const VmContext, param: i32) {
    let id = with_core_scheduler(|scheduler| scheduler.get_current_thread().id());
    println!("{:?}    os hello {} {:#p}", id, param, _vmctx);
    //arch::halt();
}

/// Compiles a WebAssembly buffer.
fn compile(buffer: &[u8]) -> Result<CompileResult, Error> {
    let isa_builder = cranelift_native::builder().unwrap();
    let mut flag_builder = settings::builder();

    // Flags
    flag_builder.set("opt_level", "speed_and_size").unwrap();
    flag_builder.set("enable_probestack", "true").unwrap();

    let flags = settings::Flags::new(flag_builder);
    let isa = isa_builder.finish(flags);

    // Module
    let mut env = ModuleEnv::new(isa.frontend_config());
    let translation = translate_module(&buffer, &mut env).map_err(Error::WasmError)?;
    let defined_function_offset = env.function_imports.len();

    // Compile the functions and store their contexts.
    let mut contexts: Vec<Context> = Vec::with_capacity(env.func_bodies.len());
    let mut total_size: usize = 0;
    for idx in 0..env.func_bodies.len() {
        let mut ctx = Context::new();
        ctx.func.signature =
            env.get_sig_from_func(FuncIndex::from_u32((idx + defined_function_offset) as u32));

        println!("{:?}", idx);

        let FunctionBody { body, offset } = env.func_bodies[idx];

        let mut func_trans = FuncTranslator::new();
        func_trans
            .translate(
                &translation,
                body,
                offset,
                &mut ctx.func,
                &mut FuncEnv::new(&env),
            )
            .map_err(Error::WasmError)?;

        // println!("{:?}", ctx.func);

        let info = ctx.compile(&*isa).map_err(Error::CodegenError)?;
        total_size += info.total_size as usize;
        contexts.push(ctx);
    }

    let start_func = env.start_func.or_else(|| match env.exports.get("_start") {
        Some(Export::Function(idx)) => Some(*idx),
        _ => None,
    });

    Ok(CompileResult {
        isa,
        contexts: contexts.into_boxed_slice(),
        memories: env.memories.into_boxed_slice(),
        data_initializers: env.data_initializers.into_boxed_slice(),
        start_func,
        function_imports: env.function_imports.into_boxed_slice(),
        tables: env.tables.into_boxed_slice(),
        table_elements: env.table_elements.into_boxed_slice(),
        globals: env.globals.into_boxed_slice(),
        total_size,
    })
}
