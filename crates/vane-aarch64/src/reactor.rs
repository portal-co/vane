//! Explicit AArch64 execution embedding.
//!
//! This wrapper keeps the existing RV64 `vane::Reactor` contract untouched.
//! Embedders supply their own [`StackHost`] state, select an entry PC, and run
//! AArch64 StackOp traces through the native interpreter.

use core::{cell::RefCell, num::NonZeroU64};

use os_ctx::{Arg, ArgCell, Ctx, SysResult};
use os_host_api::{HostApi, UnixHostAdapter};
use os_unix_emulation::{Abi, Outcome, dispatch};
use vane_arch::{
    Heat, MemWidth, StateSlot,
    interp::{Exit, StackHost, interpret},
    template::{Flags, Params, TemplateJit},
};

use crate::{state, template::aarch64::a64_emit};

/// Terminal result of [`Aarch64Reactor::run_with_host_api`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeExit {
    /// Guest process termination through `exit`/`exit_group`.
    Exit(u32),
    /// Guest control flow left the currently native-driven region.
    TailCall(u64),
    /// The generated trace trapped.
    Trap(alloc::string::String),
    /// A non-Unix `StackOp::Ecall` boundary stopped interpretation.
    Ecall,
}

/// A [`Ctx`] view over AArch64 stack-host state.
///
/// This is intentionally public so an explicitly registered direct host call
/// can inspect registers and guest buffers without acquiring a second memory
/// model. Register 31 is represented as SP; ordinary direct-call arguments
/// use X0–X7 and return values use X0.
pub struct Aarch64Ctx<'a, H: StackHost + ?Sized> {
    host: RefCell<&'a mut H>,
}

impl<'a, H: StackHost + ?Sized> Aarch64Ctx<'a, H> {
    pub fn new(host: &'a mut H) -> Self {
        Self {
            host: RefCell::new(host),
        }
    }

    fn state(reg: u8) -> StateSlot {
        if reg == 31 {
            state::sp()
        } else {
            state::x(reg)
        }
    }

    fn read_reg(&self, reg: u8) -> u64 {
        self.host.borrow_mut().load_state(&Self::state(reg))
    }

    fn write_reg(&self, reg: u8, value: u64) {
        self.host.borrow_mut().store_state(&Self::state(reg), value);
    }
}

impl<H: StackHost + ?Sized> Ctx for Aarch64Ctx<'_, H> {
    fn read(&self, addr: NonZeroU64, cell: &mut (dyn ArgCell + '_)) {
        let mut offset = 0u64;
        cell.fill(&mut core::iter::from_fn(|| {
            let byte = self.host.borrow_mut().load_mem(
                addr.get().wrapping_add(offset),
                MemWidth::B1,
                false,
            ) as u8;
            offset = offset.wrapping_add(1);
            Some(byte)
        }));
    }

    fn write(&mut self, addr: NonZeroU64, cell: &mut (dyn Arg + '_)) {
        // `os_ctx::Arg` intentionally exposes width plus integer/address
        // representation. The shared Unix path writes integer byte cells;
        // direct callbacks use the same contract for scalar buffers.
        let value = cell.as_addr().map_or(0, NonZeroU64::get);
        for offset in 0..cell.len() {
            self.host.borrow_mut().store_mem(
                addr.get().wrapping_add(offset),
                MemWidth::B1,
                (value >> (offset * 8)) & 0xff,
            );
        }
    }

    fn reg(&self, idx: u8, cell: &mut (dyn ArgCell + '_)) {
        cell.fill(&mut self.read_reg(idx).to_le_bytes().into_iter());
    }

    fn set_reg(&mut self, idx: u8, cell: &mut (dyn Arg + '_)) {
        self.write_reg(idx, cell.as_addr().map_or(0, NonZeroU64::get));
    }

    fn jalr(&mut self, addr: NonZeroU64, _cell: &mut (dyn Arg + '_)) -> SysResult {
        SysResult::Jump(addr)
    }
}

struct UnixStackHost<'a, H: StackHost + ?Sized, A: HostApi + ?Sized> {
    host: &'a mut H,
    host_api: &'a mut A,
    abi: Abi,
    exit: Option<u32>,
}

impl<H: StackHost + ?Sized, A: HostApi + ?Sized> StackHost for UnixStackHost<'_, H, A> {
    fn load_reg(&mut self, reg: u8) -> u64 {
        self.host.load_reg(reg)
    }

    fn store_reg(&mut self, reg: u8, value: u64) {
        self.host.store_reg(reg, value)
    }

    fn load_state(&mut self, slot: &StateSlot) -> u64 {
        self.host.load_state(slot)
    }

    fn store_state(&mut self, slot: &StateSlot, value: u64) {
        self.host.store_state(slot, value)
    }

    fn load_mem(&mut self, addr: u64, width: MemWidth, signed: bool) -> u64 {
        self.host.load_mem(addr, width, signed)
    }

    fn store_mem(&mut self, addr: u64, width: MemWidth, value: u64) {
        self.host.store_mem(addr, width, value)
    }

    fn byte(&mut self, addr: u64) -> u8 {
        self.host.byte(addr)
    }

    fn ecall(&mut self) -> bool {
        let mut ctx = Aarch64Ctx::new(self.host);
        let mut host = UnixHostAdapter::new(self.host_api);
        match dispatch(self.abi, &mut ctx, &mut host) {
            Outcome::Return(_) => true,
            Outcome::Exit(code) => {
                self.exit = Some(code);
                false
            }
        }
    }

    fn log(&mut self, msg: &str) {
        self.host.log(msg)
    }
}

/// Deliberately selected AArch64 trace runner for native/server embeddings.
pub struct Aarch64Reactor<'a, R> {
    /// Guest instruction source. `Mem` requires mutable page access during
    /// trace fetch, so embeddings intentionally provide an exclusive source.
    pub react: &'a mut R,
    /// Root instruction pointer for integrity-check invalidation.
    pub root: u64,
    /// Renderer flags carried into trace creation.
    pub flags: Flags,
}

impl<'a, R: vane_arch::JitCtx> Aarch64Reactor<'a, R> {
    pub fn new(react: &'a mut R) -> Self {
        Self {
            react,
            root: 0,
            flags: Flags::default(),
        }
    }

    /// Execute a trace beginning at `pc`, returning its dispatcher/host exit.
    pub fn run(&self, pc: u64, host: &mut impl StackHost) -> Exit {
        let jit = TemplateJit {
            params: Params {
                react: self.react,
                trial: &|_| Heat::New,
                #[cfg(feature = "target-js")]
                flate: &vane_arch::DebugFlate {},
                root: self.root,
                flags: self.flags,
            },
            pc,
            labels: Default::default(),
            depth: 0,
        };
        interpret(&a64_emit(&jit), host)
    }

    /// Run AArch64 guest code with an explicit, policy-carrying host API.
    ///
    /// Linux uses `x8` for the syscall number; Darwin uses `x16`. Registered
    /// [`HostApi::has_osfuncall`] addresses are compiled as trace boundaries,
    /// then called only by this native driver. This catches both a static BL
    /// and a dynamic BLR without symbol matching or browser-side interception.
    pub fn run_with_host_api<A: HostApi + ?Sized, H: StackHost + ?Sized>(
        &self,
        entry_pc: u64,
        host: &mut H,
        host_api: &mut A,
        abi: Abi,
    ) -> NativeExit {
        assert!(
            matches!(abi, Abi::LinuxAarch64 | Abi::DarwinAarch64),
            "Aarch64Reactor only accepts AArch64 Unix ABIs"
        );
        let mut pc = entry_pc;
        // `TemplateJit` queries trace ownership through an immutable callback,
        // while the generated trace needs the same API mutably for syscalls.
        // A short-lived RefCell borrow keeps those phases disjoint.
        let host_api = RefCell::new(host_api);
        loop {
            let trial = |address| {
                NonZeroU64::new(address)
                    .filter(|address| host_api.borrow().has_osfuncall(*address))
                    .map_or(Heat::New, |_| Heat::Cached)
            };
            let jit = TemplateJit {
                params: Params {
                    react: self.react,
                    trial: &trial,
                    #[cfg(feature = "target-js")]
                    flate: &vane_arch::DebugFlate {},
                    root: self.root,
                    flags: self.flags,
                },
                pc,
                labels: Default::default(),
                depth: 0,
            };
            // Trace discovery asks `trial` about explicitly registered
            // targets. Complete it before borrowing the same API mutably for
            // synchronous syscall dispatch.
            let ops = a64_emit(&jit);
            let (exit, process_exit) = {
                let mut host_api = host_api.borrow_mut();
                let mut bridge = UnixStackHost {
                    host,
                    host_api: &mut **host_api,
                    abi,
                    exit: None,
                };
                let exit = interpret(&ops, &mut bridge);
                (exit, bridge.exit)
            };
            match exit {
                Exit::Trap(message) => return NativeExit::Trap(message),
                Exit::Ecall => return process_exit.map_or(NativeExit::Ecall, NativeExit::Exit),
                // Only interpret_async produces Bail (the privileged
                // machine engine); the sync path never does.
                Exit::Bail(_) => unreachable!("Bail is only produced by interpret_async"),
                Exit::TailCall(target) => {
                    let Some(address) = NonZeroU64::new(target) else {
                        return NativeExit::TailCall(target);
                    };
                    if !host_api.borrow().has_osfuncall(address) {
                        return NativeExit::TailCall(target);
                    }
                    let mut ctx = Aarch64Ctx::new(host);
                    match host_api.borrow_mut().osfuncall(address, &mut ctx) {
                        Some(SysResult::Return) => pc = ctx.read_reg(30),
                        Some(SysResult::Jump(address)) => pc = address.get(),
                        None => return NativeExit::TailCall(target),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use core::{
        num::NonZeroU64,
        sync::atomic::{AtomicU32, AtomicU64, Ordering},
    };
    use os_host_api::{ImportManifest, LinkRecipe, TunnelResolution, TunneledHostApi};
    use std::{sync::Mutex, vec::Vec};
    use vane_arch::{Mem, StateSlot};

    struct Host {
        state: BTreeMap<StateSlot, u64>,
        code: BTreeMap<u64, u8>,
        mem: BTreeMap<u64, u8>,
    }

    impl Host {
        fn with_words(words: &[u32]) -> Self {
            Self {
                state: BTreeMap::new(),
                code: words
                    .iter()
                    .copied()
                    .enumerate()
                    .flat_map(|(offset, word)| {
                        word.to_le_bytes()
                            .into_iter()
                            .enumerate()
                            .map(move |(i, byte)| ((offset * 4 + i) as u64, byte))
                    })
                    .collect(),
                mem: BTreeMap::new(),
            }
        }
    }

    fn write_words(mem: &mut Mem, words: &[u32]) {
        for (offset, word) in words.iter().copied().enumerate() {
            for (index, byte) in word.to_le_bytes().into_iter().enumerate() {
                mem.write_byte((offset * 4 + index) as u64, byte);
            }
        }
    }

    impl StackHost for Host {
        fn load_reg(&mut self, _: u8) -> u64 {
            0
        }
        fn store_reg(&mut self, _: u8, _: u64) {}
        fn load_state(&mut self, s: &StateSlot) -> u64 {
            self.state.get(s).copied().unwrap_or(0)
        }
        fn store_state(&mut self, s: &StateSlot, v: u64) {
            self.state.insert(s.clone(), v);
        }
        fn load_mem(&mut self, addr: u64, width: vane_arch::MemWidth, _: bool) -> u64 {
            let bytes = match width {
                vane_arch::MemWidth::B1 => 1,
                vane_arch::MemWidth::B2 => 2,
                vane_arch::MemWidth::B4 => 4,
                vane_arch::MemWidth::B8 => 8,
            };
            (0..bytes).fold(0, |value, i| {
                value | ((self.mem.get(&(addr + i)).copied().unwrap_or(0) as u64) << (i * 8))
            })
        }
        fn store_mem(&mut self, addr: u64, width: vane_arch::MemWidth, value: u64) {
            let bytes = match width {
                vane_arch::MemWidth::B1 => 1,
                vane_arch::MemWidth::B2 => 2,
                vane_arch::MemWidth::B4 => 4,
                vane_arch::MemWidth::B8 => 8,
            };
            for i in 0..bytes {
                self.mem.insert(addr + i, (value >> (i * 8)) as u8);
            }
        }
        fn byte(&mut self, addr: u64) -> u8 {
            self.code.get(&addr).copied().unwrap_or(0)
        }
        fn ecall(&mut self) -> bool {
            false
        }
    }

    struct RecordingApi {
        base: TunneledHostApi,
        direct: Mutex<Vec<u64>>,
        exit_code: AtomicU32,
        jump: AtomicU64,
    }

    impl RecordingApi {
        fn new() -> Self {
            Self {
                base: TunneledHostApi::for_host(),
                direct: Mutex::new(Vec::new()),
                exit_code: AtomicU32::new(u32::MAX),
                jump: AtomicU64::new(0),
            }
        }
    }

    impl HostApi for RecordingApi {
        fn import_manifest(&self) -> ImportManifest {
            self.base.import_manifest()
        }

        fn resolve_ambient(&self, guest_name: &str) -> Option<TunnelResolution> {
            self.base.resolve_ambient(guest_name)
        }

        fn link_recipe(&self) -> LinkRecipe {
            self.base.link_recipe()
        }

        fn exit(&mut self, code: u32) -> i64 {
            self.exit_code.store(code, Ordering::Relaxed);
            0
        }

        fn has_osfuncall(&self, address: NonZeroU64) -> bool {
            address.get() == 8
        }

        fn osfuncall(&mut self, address: NonZeroU64, ctx: &mut dyn Ctx) -> Option<SysResult> {
            self.direct.lock().unwrap().push(address.get());

            let mut argument = 0u64;
            ctx.reg(0, &mut argument);
            let mut return_value = argument.wrapping_add(1);
            ctx.set_reg(0, &mut return_value);

            let memory = NonZeroU64::new(0x100).unwrap();
            let mut input = 0u64;
            ctx.read(memory, &mut input);
            let mut output = input.wrapping_add(1);
            ctx.write(memory, &mut output);

            match NonZeroU64::new(self.jump.load(Ordering::Relaxed)) {
                Some(target) => Some(SysResult::Jump(target)),
                None => Some(SysResult::Return),
            }
        }
    }

    #[test]
    fn explicit_reactor_executes_a64_without_rv64_reactor() {
        let mut mem = Mem::default();
        for (offset, word) in [0xd280_0020u32, 0xd61f_03c0].into_iter().enumerate() {
            for (i, byte) in word.to_le_bytes().into_iter().enumerate() {
                mem.write_byte((offset * 4 + i) as u64, byte);
            }
        }
        let mut host = Host::with_words(&[0xd280_0020, 0xd61f_03c0]);
        host.state.insert(crate::state::x(30), 0xfeed);
        let exit = Aarch64Reactor::new(&mut mem).run(0, &mut host);
        assert!(matches!(exit, Exit::TailCall(0xfeed)), "{exit:?}");
        assert_eq!(host.state[&crate::state::x(0)], 1);
    }

    #[test]
    fn native_linux_and_darwin_svc_use_the_shared_dispatcher() {
        let mut mem = Mem::default();
        write_words(&mut mem, &[0xd400_0001]);
        let reactor = Aarch64Reactor::new(&mut mem);

        let mut linux_host = Host::with_words(&[0xd400_0001]); // svc #0
        linux_host.state.insert(state::x(8), 93);
        linux_host.state.insert(state::x(0), 7);
        let mut linux_api = RecordingApi::new();
        assert_eq!(
            reactor.run_with_host_api(0, &mut linux_host, &mut linux_api, Abi::LinuxAarch64),
            NativeExit::Exit(7)
        );
        assert_eq!(linux_api.exit_code.load(Ordering::Relaxed), 7);

        let mut darwin_host = Host::with_words(&[0xd400_0001]); // svc #0
        darwin_host.state.insert(state::x(16), 1);
        darwin_host.state.insert(state::x(0), 9);
        let mut darwin_api = RecordingApi::new();
        assert_eq!(
            reactor.run_with_host_api(0, &mut darwin_host, &mut darwin_api, Abi::DarwinAarch64),
            NativeExit::Exit(9)
        );
        assert_eq!(darwin_api.exit_code.load(Ordering::Relaxed), 9);
    }

    #[test]
    fn native_svc_returns_enosys_and_continues_for_unknown_calls() {
        let mut mem = Mem::default();
        write_words(&mut mem, &[0xd400_0001, 0xd61f_03c0]); // svc #0; br x30
        let reactor = Aarch64Reactor::new(&mut mem);
        let mut host = Host::with_words(&[0xd400_0001, 0xd61f_03c0]);
        host.state.insert(state::x(8), 0xffff);
        host.state.insert(state::x(30), 0xfeed);
        let mut api = RecordingApi::new();

        assert_eq!(
            reactor.run_with_host_api(0, &mut host, &mut api, Abi::LinuxAarch64),
            NativeExit::TailCall(0xfeed)
        );
        assert_eq!(
            host.state[&state::x(0)],
            (-os_unix_emulation::ENOSYS) as u64
        );
    }

    #[test]
    fn native_direct_calls_are_address_based_and_can_access_context_memory() {
        let mut mem = Mem::default();
        write_words(&mut mem, &[0x9400_0002, 0xd420_0000, 0xd65f_03c0]);
        let reactor = Aarch64Reactor::new(&mut mem);
        // BL 8; BRK #0.  Address 8 is explicitly registered, so its guest
        // instruction bytes are never interpreted by the native driver.
        let mut host = Host::with_words(&[0x9400_0002, 0xd420_0000, 0xd65f_03c0]);
        host.state.insert(state::x(0), 41);
        host.store_mem(0x100, vane_arch::MemWidth::B8, 99);
        let mut api = RecordingApi::new();

        assert!(matches!(
            reactor.run_with_host_api(0, &mut host, &mut api, Abi::LinuxAarch64),
            NativeExit::Trap(_)
        ));
        assert_eq!(*api.direct.lock().unwrap(), [8]);
        assert_eq!(host.state[&state::x(0)], 42);
        assert_eq!(host.load_mem(0x100, vane_arch::MemWidth::B8, false), 100);
    }

    #[test]
    fn native_dynamic_direct_call_can_jump_to_an_explicit_guest_target() {
        let mut mem = Mem::default();
        write_words(
            &mut mem,
            &[0xd63f_0020, 0xd420_0000, 0xd65f_03c0, 0xd420_0000],
        );
        let reactor = Aarch64Reactor::new(&mut mem);
        // BLR x1; BRK #0; registered target; BRK #0 at the jump destination.
        let mut host = Host::with_words(&[0xd63f_0020, 0xd420_0000, 0xd65f_03c0, 0xd420_0000]);
        host.state.insert(state::x(1), 8);
        let mut api = RecordingApi::new();
        api.jump.store(12, Ordering::Relaxed);

        assert!(matches!(
            reactor.run_with_host_api(0, &mut host, &mut api, Abi::LinuxAarch64),
            NativeExit::Trap(_)
        ));
        assert_eq!(*api.direct.lock().unwrap(), [8]);
    }

    #[test]
    fn unregistered_dynamic_calls_keep_existing_guest_control_flow() {
        let mut mem = Mem::default();
        write_words(&mut mem, &[0xd63f_0020, 0xd420_0000]); // blr x1; brk #0
        let reactor = Aarch64Reactor::new(&mut mem);
        let mut host = Host::with_words(&[0xd63f_0020, 0xd420_0000]);
        host.state.insert(state::x(1), 12);
        let mut api = RecordingApi::new(); // only address 8 is registered

        assert_eq!(
            reactor.run_with_host_api(0, &mut host, &mut api, Abi::LinuxAarch64),
            NativeExit::TailCall(12)
        );
        assert!(api.direct.lock().unwrap().is_empty());
    }
}
