#![no_std]
//! `vane-meta-gen` — the `vane_meta!` macro that generates a `Reactor`/`Core`
//! pair for a vane target.
//!
//! Per decision **D1** in `PLAN_LINUX_WASM.md`, the macro emits *two*
//! surfaces from one definition:
//!
//! - **`js-host` (default)** — the wasm-bindgen API used by the browser
//!   package: JS-held register file, `J`/`j`/`jit_run`, `_sys`, the paging
//!   string getters, and the inline-JS helpers (`get$`, `suspend`, ...).
//! - **plain-Rust twin** (`not(js-host)`) — the same `Reactor` type with a
//!   Rust-native API (`get_page`, `jit_code`, typed paging accessors) and no
//!   `JsValue` anywhere, so the emulator builds for `wasm32-wasip1`, WASIX,
//!   and native hosts.
//!
//! The cfg gates live in the *expansion* of the macro, so they are evaluated
//! against the feature set of the crate that invokes `vane_meta!` (today:
//! `vane`, which forwards its own `js-host` feature).
#[doc(hidden)]
pub use core;
#[doc(hidden)]
pub extern crate alloc;
pub use spin;
pub use vane_arch;
#[cfg(feature = "js-host")]
pub use wasm_bindgen;

#[macro_export]
macro_rules! vane_meta {
    ($t:ident, $c:ident, $y:expr, $flate:expr, $emit:expr, $hart:ty, $step:expr) => {
        #[cfg(feature = "js-host")]
        #[$crate::wasm_bindgen::prelude::wasm_bindgen(wasm_bindgen = $crate::wasm_bindgen)]
        #[derive(Clone)]
        pub struct $t {
            _handle: (),
            core: $crate::alloc::rc::Rc<$crate::spin::Mutex<$c>>,
        }
        #[cfg(not(feature = "js-host"))]
        #[derive(Clone)]
        pub struct $t {
            _handle: (),
            core: $crate::alloc::rc::Rc<$crate::spin::Mutex<$c>>,
        }
        impl $crate::vane_arch::JitCtx for $t {
            fn bytes(&self, a: u64) -> $crate::alloc::boxed::Box<dyn Iterator<Item = u8> + '_> {
                $crate::alloc::boxed::Box::new((a..).map(move |a| self.byte(a)))
            }
            fn byte(&self, a: u64) -> u8 {
                // Instruction-fetch read for trace compilation: translate
                // when Sv39 is active (identity otherwise).
                let mut lock = self.core.lock();
                let lock = &mut *lock;
                let pa = lock.hart.translate_exec(&mut lock.mem, a).unwrap_or(a);
                lock.mem.read_byte(pa)
            }
        }
        struct $c {
            mem: $crate::vane_arch::Mem,
            /// JS-held JIT function cache (`p` in the generated code).
            #[cfg(feature = "js-host")]
            state: $crate::core::cell::OnceCell<$crate::wasm_bindgen::prelude::JsValue>,
            /// JS-held register file (`r` in the generated code; `xN` BigInt
            /// properties). Only exists under `js-host`; plain-Rust hosts
            /// keep the register file in a caller-owned `[u64; 32]`.
            #[cfg(feature = "js-host")]
            regs: $crate::core::cell::OnceCell<$crate::wasm_bindgen::prelude::JsValue>,
            /// JS host API object used for Unix syscall dispatch.
            #[cfg(feature = "js-host")]
            host_api: $crate::core::cell::OnceCell<$crate::wasm_bindgen::prelude::JsValue>,
            test_mode: bool,
            /// Shared `StackOp` trace cache keyed by guest pc (M1,
            /// PLAN_LINUX_WASM.md §4.2). `RefCell` because `interpret_async`
            /// needs only `&` access to the ops; the cache is per-Reactor and
            /// never aliased across awaits by construction (single lock
            /// scope per mutation).
            trace_cache: $crate::core::cell::RefCell<
                $crate::alloc::collections::BTreeMap<
                    u64,
                    $crate::alloc::rc::Rc<$crate::alloc::vec::Vec<$crate::vane_arch::StackOp>>,
                >,
            >,
            /// Compiled-trace registry (Weval/JIT/binary-patcher artifacts,
            /// or the default dense interpreter via `compile_trace`). Keyed by
            /// the trace's root guest pc. The dispatch loop consults this
            /// before interpreting the `trace_cache` ops — a registered
            /// callable runs instead of interpreting the StackOps.
            compiled: $crate::core::cell::RefCell<
                $crate::alloc::collections::BTreeMap<
                    u64,
                    $crate::vane_arch::interp::TraceFn<$t>,
                >,
            >,
            /// Which execution engine `run` dispatches to.
            engine: $crate::vane_arch::ExecEngine,
            /// Current guest pc: the register file lives in JS (`r`) under
            /// `js-host`, so the pc must live here for host-side engines.
            pc: u64,
            /// Code-dirty flag (set by `fence.i`): the trace cache validates
            /// and clears on the next dispatch rather than on every fence —
            /// while clear, traces/JIT functions may chain directly with no
            /// integrity comparison (PLAN_LINUX_WASM.md §9).
            code_dirty: bool,
            /// Register file for plain-Rust hosts. Under `js-host` the
            /// register file lives in the JS `r` object instead.
            #[cfg(not(feature = "js-host"))]
            regs: [u64; 32],
            /// Privileged architecture state (M3, PLAN_LINUX_WASM.md §7.2):
            /// CSRs, privilege mode, trap state, Sv39 TLB. The concrete
            /// type is the arch's `$hart` parameter (e.g.
            /// `vane_riscv::hart::HartState`).
            hart: $hart,
            /// Hook system (vane-arch::hooks): observation points at real
            /// guest symbol locations. `Rc` so the trace compiler can hold
            /// the boundary predicate without keeping the core locked.
            hooks: $crate::alloc::rc::Rc<$crate::vane_arch::hooks::HookSet>,
            /// Recorded hook firings, oldest first; drained by
            /// `take_hook_events`.
            hook_events: $crate::alloc::vec::Vec<$crate::vane_arch::hooks::HookEvent>,
            /// When true, the trace loop pauses BEFORE dispatching a hooked
            /// trace (returning `Ok(hooked_pc)`) so the driver can snapshot
            /// the machine exactly at the hooked function's entry — the
            /// trajectory fork point. Resuming runs the hooked trace once
            /// (via `skip_hook_pause_once`) and pauses again on the next
            /// visit to a hooked address.
            pause_on_hook: bool,
            /// One-shot: the hooked pc to dispatch without pausing (set by
            /// the pause, consumed by the resume).
            skip_hook_pause_once: Option<u64>,
        }

        const _: () = {
            #[cfg(not(feature = "js-host"))]
            impl $crate::vane_arch::interp::LocalReactor for $t {
                fn lr_load_reg(&self, r: u8) -> u64 {
                    self.core.lock().regs[(r % 32) as usize]
                }
                fn lr_store_reg(&self, r: u8, v: u64) {
                    let mut lock = self.core.lock();
                    let r = (r % 32) as usize;
                    lock.regs[r] = if r == 0 { 0 } else { v };
                }
                // Bulk register access for the trace engine's register
                // cache: one lock for the whole file instead of one per
                // register operand (the hot-path win).
                fn lr_load_all_regs(&self) -> [u64; 32] {
                    self.core.lock().regs
                }
                fn lr_store_all_regs(&self, regs: &[u64; 32]) {
                    let mut lock = self.core.lock();
                    lock.regs = *regs;
                    lock.regs[0] = 0; // x0 is hardwired to zero
                }
                // The plain-Rust reactor keeps the register file in
                // `Core.regs`, so the trace engine may cache it.
                fn lr_has_local_regfile(&self) -> bool {
                    true
                }
                // Execute a privileged instruction inline in a trace
                // (PLAN_TRACE_CUTOVER). The handler lives on the Reactor
                // (vane has the vane_riscv dep; vane-meta-gen re-exports only
                // vane_arch).
                fn lr_privileged(&self, pc: u64, raw: u32, regs: &mut [u64; 32]) -> $crate::vane_arch::interp::PrivOutcome {
                    self.execute_privileged_inst(pc, raw, regs)
                }
                fn lr_read_byte(&self, addr: u64) -> u8 {
                    // Fetch validation (CheckCode): translate as exec.
                    let mut lock = self.core.lock();
                    let lock = &mut *lock;
                    let pa = lock
                        .hart
                        .translate_exec(&mut lock.mem, addr)
                        .unwrap_or(addr);
                    lock.mem.read_byte(pa)
                }
                fn lr_write_byte(&self, addr: u64, v: u8) {
                    // Data write: translate + MMIO.
                    let mut lock = self.core.lock();
                    let lock = &mut *lock;
                    let pa = match lock.hart.translate_store(&mut lock.mem, addr) {
                        Ok(p) => p,
                        Err(_) => {
                            lock.hart.pending_trace_trap = true;
                            return;
                        }
                    };
                    match lock
                        .hart
                        .machine_store(pa, $crate::vane_arch::MemWidth::B1, v as u64)
                    {
                        Some(Ok(())) => {}
                        Some(Err(_)) => lock.hart.pending_trace_trap = true,
                        None => lock.mem.write_byte(pa, v),
                    }
                }
                fn lr_load_mem(&self, addr: u64, width: $crate::vane_arch::MemWidth, signed: bool) -> u64 {
                    let mut lock = self.core.lock();
                    let lock = &mut *lock;
                    let pa = match lock.hart.translate_load(&mut lock.mem, addr) {
                        Ok(p) => p,
                        Err(_) => {
                            lock.hart.pending_trace_trap = true;
                            return 0;
                        }
                    };
                    if let Some(r) = lock.hart.machine_load(pa, width, signed) {
                        return match r {
                            Ok(v) => v,
                            Err(_) => {
                                lock.hart.pending_trace_trap = true;
                                0
                            }
                        };
                    }
                    let n = match width {
                        $crate::vane_arch::MemWidth::B1 => 1,
                        $crate::vane_arch::MemWidth::B2 => 2,
                        $crate::vane_arch::MemWidth::B4 => 4,
                        $crate::vane_arch::MemWidth::B8 => 8,
                    };
                    let mut v = 0u64;
                    for i in 0..n {
                        v |= (lock.mem.read_byte(pa.wrapping_add(i)) as u64) << (8 * i);
                    }
                    if signed {
                        match width {
                            $crate::vane_arch::MemWidth::B1 => v as u8 as i8 as i64 as u64,
                            $crate::vane_arch::MemWidth::B2 => v as u16 as i16 as i64 as u64,
                            $crate::vane_arch::MemWidth::B4 => v as u32 as i32 as i64 as u64,
                            $crate::vane_arch::MemWidth::B8 => v,
                        }
                    } else {
                        v
                    }
                }
                fn lr_store_mem(&self, addr: u64, width: $crate::vane_arch::MemWidth, v: u64) {
                    let mut lock = self.core.lock();
                    let lock = &mut *lock;
                    let pa = match lock.hart.translate_store(&mut lock.mem, addr) {
                        Ok(p) => p,
                        Err(_) => {
                            lock.hart.pending_trace_trap = true;
                            return;
                        }
                    };
                    if let Some(r) = lock.hart.machine_store(pa, width, v) {
                        if r.is_err() {
                            lock.hart.pending_trace_trap = true;
                        }
                        return;
                    }
                    let n = match width {
                        $crate::vane_arch::MemWidth::B1 => 1,
                        $crate::vane_arch::MemWidth::B2 => 2,
                        $crate::vane_arch::MemWidth::B4 => 4,
                        $crate::vane_arch::MemWidth::B8 => 8,
                    };
                    for i in 0..n {
                        lock.mem.write_byte(pa.wrapping_add(i), (v >> (8 * i)) as u8);
                    }
                }
                fn lr_take_pending_trap(&self) -> bool {
                    let mut lock = self.core.lock();
                    let p = lock.hart.pending_trace_trap;
                    lock.hart.pending_trace_trap = false;
                    p
                }
                fn lr_take_irq_check(&self) -> bool {
                    let mut lock = self.core.lock();
                    let p = lock.hart.pending_irq_check;
                    lock.hart.pending_irq_check = false;
                    p
                }

            }

            impl $t {
                /// Run the guest from `pc` with the `StackOp` trace
                /// interpreter — no runtime codegen (M1, PLAN_LINUX_WASM.md
                /// §4.2). Traces are compiled with the same `$y`/`$emit`
                /// pipeline the JS JIT renders from, so both engines
                /// execute identical semantics by construction (D5).
                ///
                /// `on_ecall` observes the live host (register file +
                /// memory + pc of the instruction after the ecall) whenever
                /// the guest executes `ecall`, and returns `true` to stop.
                /// Bounded variant of [`Self::trace_run`]: returns `Ok(pc)`
                /// after `fuel` trace dispatches.
                pub async fn trace_run_fueled<F>(
                    &self,
                    pc: u64,
                    on_ecall: &mut F,
                    mut fuel: Option<u64>,
                ) -> Result<u64, String>
                where
                    F: AsyncFnMut(
                        &mut $crate::vane_arch::interp::LocalStackHost<Self>,
                    ) -> bool,
                {
                    // The register cache starts from the reactor's register
                    // file; register ops in traces hit the cache (no lock).
                    let initial_regs = $crate::vane_arch::interp::LocalReactor::lr_load_all_regs(self);
                    let (test_mode0, hart_enabled0) = {
                        let g = self.core.lock();
                        (g.test_mode, g.hart.enabled)
                    };
                    let mut host = $crate::vane_arch::interp::LocalStackHost {
                        reactor: self.clone(),
                        test_mode: test_mode0,
                        pc,
                        pending_ecall: false,
                        undo_log: $crate::alloc::vec::Vec::new(),
                        undo_mark: 0,
                        regs: initial_regs,
                        regs_dirty: false,
                        // With the privileged hart active, the fence.i/satp
                        // trace-invalidation contract covers self-modifying
                        // code, so CheckCode need not re-read code bytes.
                        check_code_integrity: !hart_enabled0,
                        // Cache the register file only when it's locally
                        // resident (not under js-host, where regs live in JS).
                        reg_cache: $crate::vane_arch::interp::LocalReactor::lr_has_local_regfile(self),
                    };
                    loop {
                        if let Some(f) = &mut fuel {
                            if *f == 0 {
                                host.flush_regs();
                                return Ok(host.pc);
                            }
                            *f -= 1;
                        }
                        // A guest poweroff stops the run.
                        if self.core.lock().hart.power_pending() {
                            host.flush_regs();
                            return Ok(host.pc);
                        }
                        // Fetch (or compile-and-cache) the trace for pc.
                        // Neither the mutex nor the RefCell borrow is held
                        // across an await.
                        let pc = host.pc;
                        // fence.i contract: revalidate code only once per
                        // fence, not per trace (see Core::code_dirty).
                        {
                            let mut lock = self.core.lock();
                            if lock.code_dirty {
                                lock.trace_cache.borrow_mut().clear();
                                lock.compiled.borrow_mut().clear();
                                lock.code_dirty = false;
                            }
                        }
                        // A cached trace is always non-privileged: privileged
                        // instructions are stepped (below) and never compiled,
                        // so a cache hit lets us skip the per-instruction peek
                        // and only deliver pending interrupts (cheap).
                        let cached_ops =
                            self.core.lock().trace_cache.borrow().get(&pc).cloned();
                        // Fresh undo log for this trace (trap atomicity).
                        host.undo_log.clear();
                        host.undo_mark = 0;
                        // The stepper reads/writes the reactor's registers, so
                        // flush the register cache first.
                        host.flush_regs();
                        let ops = if let Some(ops) = cached_ops {
                            // Deliver any pending interrupt before running the
                            // cached (non-privileged) trace — no peek needed.
                            if let Some(np) = self.deliver_pending_interrupt(host.pc)? {
                                host.load_regs();
                                host.pc = np;
                                continue;
                            }
                            ops
                        } else {
                            // Uncached pc: full privileged step (interrupts +
                            // instruction peek). Privileged instructions step
                            // here and are never compiled/cached.
                            if let Some(np) = $step(self, host.pc, false)? {
                                host.load_regs();
                                host.pc = np;
                                continue;
                            }
                            // Non-privileged: compile the trace and cache it.

                            let f = $flate;
                            let lock = self.core.lock();
                            let test_mode = lock.test_mode;
                            let paging_mode = lock.mem.paging_mode;
                            let spt = lock.mem.shared_page_table_vaddr;
                            let ssd = lock.mem.shared_security_directory_vaddr;
                            let u32p = lock.mem.use_32bit_paging;
                            let mlp = lock.mem.use_multilevel_paging;
                            let cache_hit = lock.trace_cache.borrow().contains_key(&pc);
                            let hart_enabled = lock.hart.enabled;
                            let hooks = lock.hooks.clone();
                            drop(lock);
                            let mut flags = $crate::vane_arch::template::Flags::with_paging(
                                test_mode,
                                paging_mode,
                                spt,
                                ssd,
                                u32p,
                                mlp,
                            );
                            // Terminate every trace right after an ecall so
                            // the deferred handler below runs at a block
                            // boundary (see LocalStackHost::ecall).
                            flags.ecall_boundary = true;
                            // Under the privileged architecture (M3),
                            // privileged instructions become trace
                            // boundaries handled by the dispatch stepper.
                            flags.privileged = hart_enabled;
                            // Hook boundaries: hooked addresses must start
                            // their own trace (vane-arch::hooks), so the
                            // emitter consults the installed set.
                            let boundary = |a: u64| hooks.is_boundary(a);
                            let jit = $crate::vane_arch::template::TemplateJit {
                                params: Params {
                                    react: self,
                                    trial: &|_| {
                                        if cache_hit {
                                            $crate::vane_arch::Heat::Cached
                                        } else {
                                            $crate::vane_arch::Heat::New
                                        }
                                    },
                                    root: pc,
                                    flate: &f,
                                    flags,
                                    boundaries: if hooks.is_empty() {
                                        None
                                    } else {
                                        Some(&boundary)
                                    },
                                },
                                pc,
                                labels: $crate::vane_arch::template::Labels::default(),
                                depth: 0,
                            };
                            let ops = $crate::alloc::rc::Rc::new($emit(&jit));
                            self.core
                                .lock()
                                .trace_cache
                                .borrow_mut()
                                .insert(pc, ops.clone());
                            ops

                        };
                        let pc = host.pc;
                        let mut lock = self.core.lock();
                        // Hook system: a trace starting at a hooked address
                        // fires an event carrying the live argument
                        // registers (a0-a7 — the hook sits at a real symbol
                        // location, so these are the real arguments). Under
                        // `js-host` the register cache is not authoritative;
                        // hook args there are best-effort.
                        let hook = lock.hooks.at(pc).map(|h| (h.name.clone(), h.addr));
                        if let Some((name, addr)) = hook {
                            let mut args = [0u64; 8];
                            args.copy_from_slice(&host.regs[10..18]);
                            lock.hook_events.push($crate::vane_arch::hooks::HookEvent {
                                name,
                                addr,
                                pc,
                                args,
                            });
                            if lock.pause_on_hook {
                                if lock.skip_hook_pause_once == Some(pc) {
                                    // The resume after a pause: run this
                                    // hooked trace once, re-arm afterwards.
                                    lock.skip_hook_pause_once = None;
                                } else {
                                    // Pause exactly at the hooked function's
                                    // entry, before any of it executes.
                                    lock.skip_hook_pause_once = Some(pc);
                                    host.flush_regs();
                                    lock.pc = host.pc;
                                    return Ok(host.pc);
                                }
                            }
                        }
                        // Compiled-trace registry: a registered callable
                        // (Weval/JIT/patcher artifact, or the default compiled
                        // interpreter) runs the trace instead of interpreting
                        // the StackOps. The callable is synchronous.
                        let compiled = lock.compiled.borrow().get(&pc).cloned();
                        drop(lock);
                        let exit = if let Some(cf) = compiled {
                            cf(&mut host)
                        } else {
                            // Per-trace op budget: bounds how long a trace
                            // runs before yielding to the dispatch loop
                            // (interrupt delivery, ecall handling, fuel).
                            let mut op_fuel: u64 = 4096;
                            $crate::vane_arch::interp::interpret_async_fueled(
                                &ops,
                                &mut host,
                                Some(&mut op_fuel),
                            )
                            .await
                            .expect("a fuel budget was supplied, so FuelExhausted is unreachable")
                        };
                        match exit {
                            $crate::vane_arch::interp::Exit::TailCall(next) => {
                                host.pc = next;
                                if host.pending_ecall {
                                    host.pending_ecall = false;
                                    if on_ecall(&mut host).await {
                                        host.flush_regs();
                                        self.core.lock().pc = host.pc;
                                        return Ok(host.pc);
                                    }
                                }
                            }
                            $crate::vane_arch::interp::Exit::Trap(msg) => {
                                host.flush_regs();
                                self.core.lock().pc = host.pc;
                                return Err(msg);
                            }
                            $crate::vane_arch::interp::Exit::Bail(fault_pc) => {
                                // A memory access trapped mid-trace. Undo
                                // the faulting instruction's register writes
                                // (earlier instructions' writes stay) and
                                // re-run it through the stepper, which
                                // raises the trap with correct state.
                                host.undo_pending_inst();
                                // Flush the undone cache so the forced
                                // re-step sees the pre-instruction state,
                                // then reload the cache from the result.
                                host.flush_regs();
                                match $step(self, fault_pc, true)? {
                                    Some(np) => {
                                        host.load_regs();
                                        host.pc = np;
                                    }
                                    None => host.pc = fault_pc,
                                }
                            }
                            $crate::vane_arch::interp::Exit::Ecall => {
                                // LocalStackHost::ecall never stops a trace.
                                self.core.lock().pc = host.pc;
                                return Err("internal: unexpected Ecall exit".into());
                            }
                        }
                    }
                }

                /// Run the guest from `pc` on the `StackOp` trace engine
                /// (unbounded). See [`Self::trace_run_fueled`].
                pub async fn trace_run<F>(
                    &self,
                    pc: u64,
                    on_ecall: &mut F,
                ) -> Result<u64, String>
                where
                    F: AsyncFnMut(
                        &mut $crate::vane_arch::interp::LocalStackHost<Self>,
                    ) -> bool,
                {
                    self.trace_run_fueled(pc, on_ecall, None).await
                }

                /// Which execution engine `run` dispatches to.
                pub fn engine(&self) -> $crate::vane_arch::ExecEngine {
                    self.core.lock().engine
                }

                /// Select the execution engine.
                pub fn set_engine(&self, engine: $crate::vane_arch::ExecEngine) {
                    self.core.lock().engine = engine;
                }

                /// Install the hook set (vane-arch::hooks): observation
                /// points at real guest symbol locations. Installing hooks
                /// invalidates the trace caches — previously compiled
                /// traces may span addresses that are boundaries now.
                pub fn install_hooks(&self, hooks: $crate::vane_arch::hooks::HookSet) {
                    let mut lock = self.core.lock();
                    lock.hooks = $crate::alloc::rc::Rc::new(hooks);
                    lock.trace_cache.borrow_mut().clear();
                    lock.compiled.borrow_mut().clear();
                    lock.code_dirty = false;
                }

                /// Number of installed hooks.
                pub fn hooks_len(&self) -> usize {
                    self.core.lock().hooks.len()
                }

                /// Drain the recorded hook events (oldest first).
                pub fn take_hook_events(
                    &self,
                ) -> $crate::alloc::vec::Vec<$crate::vane_arch::hooks::HookEvent> {
                    $crate::core::mem::take(&mut self.core.lock().hook_events)
                }

                /// Enable/disable pausing before hooked traces (see
                /// `pause_on_hook`): the trajectory fork-point mechanism.
                pub fn set_pause_on_hook(&self, enabled: bool) {
                    let mut lock = self.core.lock();
                    lock.pause_on_hook = enabled;
                    if !enabled {
                        lock.skip_hook_pause_once = None;
                    }
                }

                /// When the trace loop paused before a hooked trace, the
                /// hooked pc (also the returned `Ok(pc)`). `None` when not
                /// paused at a hook. A driver distinguishes a hook pause
                /// from fuel exhaustion with this.
                pub fn hook_pause_pending(&self) -> Option<u64> {
                    self.core.lock().skip_hook_pause_once
                }
            }
        };

        /// Plain-Rust twin surface — emitted only for non-JS hosts (the
        /// `js-host` build gets the same operations through the
        /// wasm-bindgen impl below, which is also callable from Rust).
        #[cfg(not(feature = "js-host"))]
        impl $t {
            /// Raw pointer into the 64 KiB guest page containing `a`,
            /// allocating the page on demand.
            pub fn get_page(&self, a: u64) -> *mut u8 {
                self.core.lock().mem.get_page(a)
            }

            /// Whether test-mode HINT logging is enabled.
            pub fn test_mode(&self) -> bool {
                self.core.lock().test_mode
            }

            /// Enable or disable test mode.
            pub fn set_test_mode(&self, value: bool) {
                self.core.lock().test_mode = value;
            }

            /// Typed accessor for the memory paging mode.
            pub fn paging_mode(&self) -> $crate::vane_arch::PagingMode {
                self.core.lock().mem.paging_mode
            }

            /// Set the memory paging mode.
            pub fn set_paging_mode(&self, mode: $crate::vane_arch::PagingMode) {
                self.core.lock().mem.paging_mode = mode;
            }

            /// Shared page-table virtual address (Shared/Both modes).
            pub fn shared_page_table_vaddr(&self) -> Option<u64> {
                self.core.lock().mem.shared_page_table_vaddr
            }

            /// Set the shared page-table virtual address.
            pub fn set_shared_page_table_vaddr(&self, addr: Option<u64>) {
                self.core.lock().mem.shared_page_table_vaddr = addr;
            }

            /// Shared security-directory virtual address.
            pub fn shared_security_directory_vaddr(&self) -> Option<u64> {
                self.core.lock().mem.shared_security_directory_vaddr
            }

            /// Set the shared security-directory virtual address.
            pub fn set_shared_security_directory_vaddr(&self, addr: Option<u64>) {
                self.core.lock().mem.shared_security_directory_vaddr = addr;
            }

            /// Whether 32-bit physical addressing is used.
            pub fn use_32bit_paging(&self) -> bool {
                self.core.lock().mem.use_32bit_paging
            }

            /// Select 32-bit physical addressing.
            pub fn set_use_32bit_paging(&self, value: bool) {
                self.core.lock().mem.use_32bit_paging = value;
            }

            /// Whether multi-level paging is used.
            pub fn use_multilevel_paging(&self) -> bool {
                self.core.lock().mem.use_multilevel_paging
            }

            /// Select multi-level paging.
            pub fn set_use_multilevel_paging(&self, value: bool) {
                self.core.lock().mem.use_multilevel_paging = value;
            }

            /// Read one guest-memory byte (image loading / inspection).
            pub fn read_byte(&self, addr: u64) -> u8 {
                self.core.lock().mem.read_byte(addr)
            }

            /// Write one guest-memory byte (image loading).
            pub fn write_byte(&self, addr: u64, value: u8) {
                self.core.lock().mem.write_byte(addr, value);
            }

            /// Generate the JavaScript source for the trace rooted at guest
            /// pc `a`, assuming no cached traces (`Heat::New`). Code
            /// generation itself is host-independent; only *execution* of
            /// the generated source needs a JS engine.
            pub fn jit_code(&self, a: u64) -> String {
                let f = $flate;
                let lock = self.core.lock();
                let test_mode = lock.test_mode;
                let paging_mode = lock.mem.paging_mode;
                let shared_page_table_vaddr = lock.mem.shared_page_table_vaddr;
                let shared_security_directory_vaddr = lock.mem.shared_security_directory_vaddr;
                let use_32bit_paging = lock.mem.use_32bit_paging;
                let use_multilevel_paging = lock.mem.use_multilevel_paging;
                drop(lock);

                let flags = $crate::vane_arch::template::Flags::with_paging(
                    test_mode,
                    paging_mode,
                    shared_page_table_vaddr,
                    shared_security_directory_vaddr,
                    use_32bit_paging,
                    use_multilevel_paging,
                );

                return ($crate::vane_arch::template::CoreJS {
                    content: &$y(&$crate::vane_arch::template::TemplateJit {
                        params: Params {
                            react: self,
                            trial: &|_| $crate::vane_arch::Heat::New,
                            root: a,
                            flate: &f,
                            flags,
                            boundaries: None,
                        },
                        pc: a,
                        labels: $crate::vane_arch::template::Labels::default(),
                        depth: 0,
                    }),
                    flate: &f,
                    flags,
                }
                .to_string());
            }
        }

        #[cfg(feature = "js-host")]
        const _: () = {
            #[$crate::wasm_bindgen::prelude::wasm_bindgen(inline_js = r#"
    const suspend = a=>{
        try{
            return (a._p??=a.p).s??=new WebAssembly.Suspending(async b=>await l(get(a,b)));
        }catch{
            return;
        };
    };
    export function get$(a,b){
        const jit = () => {
            let code;
            try{
                return (new (get$.f ??= Function)("$","J",code = a.j(b))(a,b=>get$(a,b)))
            }catch(err){
                console.error(err);
                console.info('code:',code);
                return a.interp.bind(a,b);
            }
        };
        return a.p[`${b}`]??=jit();
    }
    export function on(){
        return Object.create(null)
    }
    export function tget(a,b){
        return (a._p??=a.p)[`${b}`]
    }
    export async function l(a){
        while(typeof a === "function")a = await a();
        return a;
    }
    export function reg(a,b){
        b %= 32;
        if(!b)return 0n;
        return (a._r??=a.r)[`x${b}`]??=0n;
    }
    export function set_reg(a,b,c){
        b %= 32;
        if(!b)return c;
        return (a._r??=a.r)[`x${b}`]=c;
    }
    export function get_memory(wasm){
        return wasm.memory;
    }

    export async function jit_run(a){
        try{
        return await a();
        }catch(err){
        console.error(err);
        throw err;
        }
    }
    "#,wasm_bindgen = $crate::wasm_bindgen)]
            extern "C" {
                #[wasm_bindgen(js_name = "get$")]
                fn get(a: $t, b: u64) -> $crate::wasm_bindgen::prelude::JsValue;
                fn tget(a: $t, b: u64) -> $crate::wasm_bindgen::prelude::JsValue;
                fn on() -> $crate::wasm_bindgen::prelude::JsValue;
                fn reg(a: $t, b: u8) -> u64;
                fn set_reg(a: $t, b: u8, c: u64) -> u64;
                fn get_memory(
                    a: $crate::wasm_bindgen::prelude::JsValue,
                ) -> $crate::wasm_bindgen::prelude::JsValue;

                #[wasm_bindgen(catch)]
                async fn jit_run(
                    a: $crate::wasm_bindgen::prelude::JsValue,
                ) -> Result<
                    $crate::wasm_bindgen::prelude::JsValue,
                    $crate::wasm_bindgen::prelude::JsValue,
                >;
                //   #[wasm_bindgen(thread_local_v2, js_name = "memory")]
                // static MEM_HANDLE: $crate::wasm_bindgen::prelude::JsValue;
            }
            #[$crate::wasm_bindgen::prelude::wasm_bindgen(wasm_bindgen = $crate::wasm_bindgen)]
            extern "C" {
                fn eval(a: &str) -> $crate::wasm_bindgen::prelude::JsValue;
            }
            impl $t {
                fn reg(self, b: u8) -> u64 {
                    reg(self, b)
                }
                fn set_reg(self, a: u8, b: u64) -> u64 {
                    set_reg(self, a, b)
                }
            }
            impl $crate::vane_arch::interp::LocalReactor for $t {
                fn lr_load_reg(&self, r: u8) -> u64 {
                    reg(self.clone(), r)
                }
                fn lr_store_reg(&self, r: u8, v: u64) {
                    set_reg(self.clone(), r, v);
                }
                fn lr_read_byte(&self, addr: u64) -> u8 {
                    // Fetch validation (CheckCode): translate as exec.
                    let mut lock = self.core.lock();
                    let lock = &mut *lock;
                    let pa = lock
                        .hart
                        .translate_exec(&mut lock.mem, addr)
                        .unwrap_or(addr);
                    lock.mem.read_byte(pa)
                }
                fn lr_write_byte(&self, addr: u64, v: u8) {
                    // Data write: translate + MMIO.
                    let mut lock = self.core.lock();
                    let lock = &mut *lock;
                    let pa = match lock.hart.translate_store(&mut lock.mem, addr) {
                        Ok(p) => p,
                        Err(_) => {
                            lock.hart.pending_trace_trap = true;
                            return;
                        }
                    };
                    match lock
                        .hart
                        .machine_store(pa, $crate::vane_arch::MemWidth::B1, v as u64)
                    {
                        Some(Ok(())) => {}
                        Some(Err(_)) => lock.hart.pending_trace_trap = true,
                        None => lock.mem.write_byte(pa, v),
                    }
                }
                fn lr_load_mem(&self, addr: u64, width: $crate::vane_arch::MemWidth, signed: bool) -> u64 {
                    let mut lock = self.core.lock();
                    let lock = &mut *lock;
                    let pa = match lock.hart.translate_load(&mut lock.mem, addr) {
                        Ok(p) => p,
                        Err(_) => {
                            lock.hart.pending_trace_trap = true;
                            return 0;
                        }
                    };
                    if let Some(r) = lock.hart.machine_load(pa, width, signed) {
                        return match r {
                            Ok(v) => v,
                            Err(_) => {
                                lock.hart.pending_trace_trap = true;
                                0
                            }
                        };
                    }
                    let n = match width {
                        $crate::vane_arch::MemWidth::B1 => 1,
                        $crate::vane_arch::MemWidth::B2 => 2,
                        $crate::vane_arch::MemWidth::B4 => 4,
                        $crate::vane_arch::MemWidth::B8 => 8,
                    };
                    let mut v = 0u64;
                    for i in 0..n {
                        v |= (lock.mem.read_byte(pa.wrapping_add(i)) as u64) << (8 * i);
                    }
                    if signed {
                        match width {
                            $crate::vane_arch::MemWidth::B1 => v as u8 as i8 as i64 as u64,
                            $crate::vane_arch::MemWidth::B2 => v as u16 as i16 as i64 as u64,
                            $crate::vane_arch::MemWidth::B4 => v as u32 as i32 as i64 as u64,
                            $crate::vane_arch::MemWidth::B8 => v,
                        }
                    } else {
                        v
                    }
                }
                fn lr_store_mem(&self, addr: u64, width: $crate::vane_arch::MemWidth, v: u64) {
                    let mut lock = self.core.lock();
                    let lock = &mut *lock;
                    let pa = match lock.hart.translate_store(&mut lock.mem, addr) {
                        Ok(p) => p,
                        Err(_) => {
                            lock.hart.pending_trace_trap = true;
                            return;
                        }
                    };
                    if let Some(r) = lock.hart.machine_store(pa, width, v) {
                        if r.is_err() {
                            lock.hart.pending_trace_trap = true;
                        }
                        return;
                    }
                    let n = match width {
                        $crate::vane_arch::MemWidth::B1 => 1,
                        $crate::vane_arch::MemWidth::B2 => 2,
                        $crate::vane_arch::MemWidth::B4 => 4,
                        $crate::vane_arch::MemWidth::B8 => 8,
                    };
                    for i in 0..n {
                        lock.mem.write_byte(pa.wrapping_add(i), (v >> (8 * i)) as u8);
                    }
                }
                fn lr_take_pending_trap(&self) -> bool {
                    let mut lock = self.core.lock();
                    let p = lock.hart.pending_trace_trap;
                    lock.hart.pending_trace_trap = false;
                    p
                }
                fn lr_take_irq_check(&self) -> bool {
                    let mut lock = self.core.lock();
                    let p = lock.hart.pending_irq_check;
                    lock.hart.pending_irq_check = false;
                    p
                }

            }
            #[$crate::wasm_bindgen::prelude::wasm_bindgen(wasm_bindgen = $crate::wasm_bindgen)]
            impl $t {
                #[wasm_bindgen(getter, js_name = "p",wasm_bindgen = $crate::wasm_bindgen)]
                pub fn state(&self) -> $crate::wasm_bindgen::prelude::JsValue {
                    let mut lock = self.core.lock();
                    return lock.state.get_or_init(|| on()).clone();
                }
                #[wasm_bindgen(getter, js_name = "r",wasm_bindgen = $crate::wasm_bindgen)]
                pub fn regs(&self) -> $crate::wasm_bindgen::prelude::JsValue {
                    let mut lock = self.core.lock();
                    return lock.regs.get_or_init(|| on()).clone();
                }
                #[wasm_bindgen(getter, js_name = "test_mode",wasm_bindgen = $crate::wasm_bindgen)]
                pub fn get_test_mode(&self) -> bool {
                    self.core.lock().test_mode
                }
                #[wasm_bindgen(setter, js_name = "test_mode",wasm_bindgen = $crate::wasm_bindgen)]
                pub fn set_test_mode(&self, value: bool) {
                    self.core.lock().test_mode = value;
                }

                /// Configure the mandatory explicit host API used for Unix
                /// syscalls. The object may implement async `read`, `write`,
                /// `close`, and `exit` methods; absent methods return ENOSYS.
                #[wasm_bindgen(js_name = "set_host_api",wasm_bindgen = $crate::wasm_bindgen)]
                pub fn set_host_api(&self, host: $crate::wasm_bindgen::prelude::JsValue) -> Result<(), $crate::wasm_bindgen::prelude::JsValue> {
                    self.core.lock().host_api.set(host).map_err(|_| $crate::wasm_bindgen::prelude::JsValue::from_str("vane host API is already configured"))
                }

                #[wasm_bindgen(js_name = "get_paging_mode",wasm_bindgen = $crate::wasm_bindgen)]
                pub fn get_paging_mode(&self) -> String {
                    match self.core.lock().mem.paging_mode {
                        $crate::vane_arch::PagingMode::Legacy => "legacy".to_string(),
                        $crate::vane_arch::PagingMode::Shared => "shared".to_string(),
                        $crate::vane_arch::PagingMode::Both => "both".to_string(),
                    }
                }

                #[wasm_bindgen(js_name = "set_paging_mode",wasm_bindgen = $crate::wasm_bindgen)]
                pub fn set_paging_mode(&self, mode: &str) {
                    let paging_mode = match mode {
                        "legacy" => $crate::vane_arch::PagingMode::Legacy,
                        "shared" => $crate::vane_arch::PagingMode::Shared,
                        "both" => $crate::vane_arch::PagingMode::Both,
                        _ => $crate::vane_arch::PagingMode::Legacy,
                    };
                    self.core.lock().mem.paging_mode = paging_mode;
                }

                #[wasm_bindgen(js_name = "get_shared_page_table_vaddr",wasm_bindgen = $crate::wasm_bindgen)]
                pub fn get_shared_page_table_vaddr(&self) -> Option<u64> {
                    self.core.lock().mem.shared_page_table_vaddr
                }

                #[wasm_bindgen(js_name = "set_shared_page_table_vaddr",wasm_bindgen = $crate::wasm_bindgen)]
                pub fn set_shared_page_table_vaddr(&self, addr: Option<u64>) {
                    self.core.lock().mem.shared_page_table_vaddr = addr;
                }

                #[wasm_bindgen(js_name = "get_shared_security_directory_vaddr",wasm_bindgen = $crate::wasm_bindgen)]
                pub fn get_shared_security_directory_vaddr(&self) -> Option<u64> {
                    self.core.lock().mem.shared_security_directory_vaddr
                }

                #[wasm_bindgen(js_name = "set_shared_security_directory_vaddr",wasm_bindgen = $crate::wasm_bindgen)]
                pub fn set_shared_security_directory_vaddr(&self, addr: Option<u64>) {
                    self.core.lock().mem.shared_security_directory_vaddr = addr;
                }

                #[wasm_bindgen(js_name = "get_use_32bit_paging",wasm_bindgen = $crate::wasm_bindgen)]
                pub fn get_use_32bit_paging(&self) -> bool {
                    self.core.lock().mem.use_32bit_paging
                }

                #[wasm_bindgen(js_name = "set_use_32bit_paging",wasm_bindgen = $crate::wasm_bindgen)]
                pub fn set_use_32bit_paging(&self, value: bool) {
                    self.core.lock().mem.use_32bit_paging = value;
                }

                #[wasm_bindgen(js_name = "get_use_multilevel_paging",wasm_bindgen = $crate::wasm_bindgen)]
                pub fn get_use_multilevel_paging(&self) -> bool {
                    self.core.lock().mem.use_multilevel_paging
                }

                #[wasm_bindgen(js_name = "set_use_multilevel_paging",wasm_bindgen = $crate::wasm_bindgen)]
                pub fn set_use_multilevel_paging(&self, value: bool) {
                    self.core.lock().mem.use_multilevel_paging = value;
                }

                #[wasm_bindgen(wasm_bindgen = $crate::wasm_bindgen)]
                pub fn _sys(&self, a: &str) -> $crate::wasm_bindgen::prelude::JsValue {
                    match a {
                        "memory" => get_memory(eval("wasm")),
                        _ => $crate::wasm_bindgen::prelude::JsValue::undefined(),
                    }
                }
                #[wasm_bindgen(wasm_bindgen = $crate::wasm_bindgen)]
                pub fn get_page(&self, a: u64) -> *mut u8 {
                    let mut lock = self.core.lock();
                    match &mut lock.mem {
                        m => m.get_page(a),
                    }
                }
                #[wasm_bindgen(js_name = "J",wasm_bindgen = $crate::wasm_bindgen)]
                pub fn jit(&self, a: u64) -> $crate::wasm_bindgen::prelude::JsValue {
                    return get(self.clone(), a);
                }
                #[wasm_bindgen]
                pub async fn jit_run(
                    &self,
                    mut pc: u64,
                ) -> Result<
                    $crate::wasm_bindgen::prelude::JsValue,
                    $crate::wasm_bindgen::prelude::JsValue,
                > {
                    let j = self.jit(pc);
                    return jit_run(j).await;
                }
                #[wasm_bindgen(js_name = "j",wasm_bindgen = $crate::wasm_bindgen)]
                pub fn jit_code(&self, a: u64) -> String {
                    let f = $flate;
                    let lock = self.core.lock();
                    let test_mode = lock.test_mode;
                    let paging_mode = lock.mem.paging_mode;
                    let shared_page_table_vaddr = lock.mem.shared_page_table_vaddr;
                    let shared_security_directory_vaddr = lock.mem.shared_security_directory_vaddr;
                    let use_32bit_paging = lock.mem.use_32bit_paging;
                    let use_multilevel_paging = lock.mem.use_multilevel_paging;
                    drop(lock);

                    let flags = $crate::vane_arch::template::Flags::with_paging(
                        test_mode,
                        paging_mode,
                        shared_page_table_vaddr,
                        shared_security_directory_vaddr,
                        use_32bit_paging,
                        use_multilevel_paging,
                    );

                    return ($crate::vane_arch::template::CoreJS {
                        content: &$y(&$crate::vane_arch::template::TemplateJit {
                            params: Params {
                                react: self,
                                trial: &|a| match tget(self.clone(), a)
                                    != $crate::wasm_bindgen::prelude::JsValue::UNDEFINED
                                {
                                    true => $crate::vane_arch::Heat::Cached,
                                    false => $crate::vane_arch::Heat::New,
                                },
                                root: a,
                                flate: &f,
                                flags,
                                boundaries: None,
                            },
                            pc: a,
                            labels: $crate::vane_arch::template::Labels::default(),
                            depth: 0,
                        }),
                        flate: &f,
                        flags,
                    }
                    .to_string());
                }
                #[wasm_bindgen(getter, js_name = "f",wasm_bindgen = $crate::wasm_bindgen)]
                pub fn u64_max(&self) -> u64 {
                    u64::MAX
                }
            }
        };
    };
}
