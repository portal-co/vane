use vane_target_core::{CoreOpcode, FloatBinOp, FloatUnaryOp, FloatWidth, MemWidth, StackOp};
#[cfg(feature = "target-wasm")]
use wasmparser::Operator;

use crate::*;

#[cfg(feature = "target-js")]
use crate::{Flate, PagingMode};

#[cfg(feature = "target-js")]
use alloc::string::{String, ToString};
#[cfg(feature = "target-js")]
use portal_jit_host_names::{CanonicalHostMethodNames, HostMethodNames, PropertyAccess};

#[cfg(feature = "target-js")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VaneHostMethod {
    Registers,
    RegisterCache,
    JitCache,
    Max64,
    GetPage,
    System,
    Ecall,
    RegRead,
    RegWrite,
    MemRead,
    MemWrite,
}

#[cfg(feature = "target-js")]
impl Display for VaneHostMethod {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Registers => "r",
            Self::RegisterCache => "_r",
            Self::JitCache => "p",
            Self::Max64 => "f",
            Self::GetPage => "get_page",
            Self::System => "_sys",
            Self::Ecall => "ecall",
            Self::RegRead => "reg_read",
            Self::RegWrite => "reg_write",
            Self::MemRead => "mem_read",
            Self::MemWrite => "mem_write",
        })
    }
}

#[cfg(feature = "target-js")]
pub fn validate_host_method_names<N: HostMethodNames<VaneHostMethod>>(
    names: &N,
) -> Result<(), String> {
    for method in [
        VaneHostMethod::Registers,
        VaneHostMethod::RegisterCache,
        VaneHostMethod::JitCache,
        VaneHostMethod::Max64,
        VaneHostMethod::GetPage,
        VaneHostMethod::System,
        VaneHostMethod::Ecall,
        VaneHostMethod::RegRead,
        VaneHostMethod::RegWrite,
        VaneHostMethod::MemRead,
        VaneHostMethod::MemWrite,
    ] {
        names.property(method).map_err(|err| err.to_string())?;
    }
    Ok(())
}

#[cfg(feature = "target-js")]
fn host_member<N: HostMethodNames<VaneHostMethod>>(
    names: &N,
    method: VaneHostMethod,
) -> alloc::string::String {
    names
        .property(method)
        .expect("Vane host names must be validated")
        .on("$")
}

/// Return a JavaScript string literal for an internal state-slot name.
/// State slots are architecture-defined, but quote defensively so a custom
/// slot can never alter generated source.
#[cfg(feature = "target-js")]
fn js_string_literal(input: &str) -> alloc::string::String {
    let mut output = alloc::string::String::from("\"");
    for ch in input.chars() {
        match ch {
            '\\' => output.push_str("\\\\"),
            '\"' => output.push_str("\\\""),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch if ch.is_control() => {
                use core::fmt::Write;
                write!(output, "\\u{:04x}", ch as u32).expect("writing to String cannot fail");
            }
            ch => output.push(ch),
        }
    }
    output.push('\"');
    output
}

/// A single named loop label.  Owned strings so the label map can be cloned
/// cheaply without lifetime constraints on the containing types.
#[derive(Clone)]
pub struct Label {
    pub ident_name: alloc::string::String,
    pub index: u32,
}

/// Map from guest PC to the loop label opened at that PC.
#[derive(Clone, Default)]
pub struct Labels(pub BTreeMap<u64, Label>);

#[derive(Clone, Copy)]
pub struct Params<'a> {
    pub react: &'a (dyn JitCtx + 'a),
    pub trial: &'a (dyn Fn(u64) -> Heat + 'a),
    #[cfg(feature = "target-js")]
    pub flate: &'a (dyn Flate + 'a),
    pub root: u64,
    pub flags: Flags,
}

#[derive(Clone, Copy, Default)]
#[non_exhaustive]
pub struct Flags {
    pub test_mode: bool,
    /// The guest is running with the privileged architecture live
    /// (PLAN_LINUX_WASM.md §7). Privileged instructions (CSR ops, xRET, WFI,
    /// SFENCE.VMA, ECALL, EBREAK) are lowered to trace boundaries
    /// (`PushImm(pc); TailCall`) so the host dispatch loop can execute them
    /// with full trap/CSR semantics instead of lowering them inline.
    /// Default false: legacy userspace-ABI behavior (ecall → host hook).
    pub privileged: bool,
    /// End every basic block immediately after an `ecall` (`Ecall`, then
    /// `PushImm(fallthrough)`, `TailCall`) instead of inlining the
    /// fallthrough into the same block. For host-side interpreters whose
    /// syscall effects (fork/exec/wait) must be applied at a *block
    /// boundary* with full access to the emulator's process state: the
    /// ecall handler records a request, the interpreter returns at the
    /// TailCall, and the driver applies the effect before emitting the
    /// next block. Default false: every existing consumer's traces are
    /// unchanged.
    pub ecall_boundary: bool,
    #[cfg(feature = "target-js")]
    pub paging_mode: Option<PagingMode>,
    #[cfg(feature = "target-js")]
    pub shared_page_table_vaddr: Option<u64>,
    #[cfg(feature = "target-js")]
    pub shared_security_directory_vaddr: Option<u64>,
    #[cfg(feature = "target-js")]
    pub use_32bit_paging: bool,
    #[cfg(feature = "target-js")]
    pub use_multilevel_paging: bool,
    /// Lower `StackOp::LoadReg`/`StoreReg`/`LoadMem`/`StoreMem` to `await
    /// $.reg_read(..)`/`$.reg_write(..)`/`$.mem_read(..)`/`$.mem_write(..)`
    /// calls instead of the default inline `DataView`/`$.r` expressions.
    ///
    /// Defaults to `false`, which reproduces today's codegen byte-for-byte —
    /// this only matters for hosts (e.g. `vane-wsdom`) whose register/memory
    /// state may not be locally resident and need a chance to `await` a
    /// round-trip. `render_stack_to_js` never decides *when* such a
    /// round-trip is actually needed — it just emits the `await` call
    /// unconditionally when this flag is set; the `$.reg_read`/`$.mem_read`
    /// host implementation is what resolves immediately if its local shadow
    /// is already fresh, or performs a real fetch if not (wsdom's own
    /// roundtrip-free-unless-needed philosophy, applied to register/memory
    /// access).
    #[cfg(feature = "target-js")]
    pub async_mem: bool,
}

impl Flags {
    /// Create a new Flags with the specified test_mode value.
    /// This is the preferred way to create Flags with non-default values
    /// since the struct is non-exhaustive.
    pub fn new_with_test_mode(test_mode: bool) -> Self {
        Self {
            test_mode,
            ..Default::default()
        }
    }

    /// Create Flags with paging configuration
    #[cfg(feature = "target-js")]
    pub fn with_paging(
        test_mode: bool,
        paging_mode: PagingMode,
        shared_page_table_vaddr: Option<u64>,
        shared_security_directory_vaddr: Option<u64>,
        use_32bit_paging: bool,
        use_multilevel_paging: bool,
    ) -> Self {
        Self {
            test_mode,
            privileged: false,
            ecall_boundary: false,
            paging_mode: Some(paging_mode),
            shared_page_table_vaddr,
            shared_security_directory_vaddr,
            use_32bit_paging,
            use_multilevel_paging,
            async_mem: false,
        }
    }
}

pub struct TemplateJit<'a> {
    pub params: Params<'a>,
    pub pc: u64,
    /// Label map accumulated so far.  Owned so sub-JIT calls can clone and
    /// extend it without borrowing the parent.
    pub labels: Labels,
    pub depth: u32,
}

/// Arch-target dispatch trait.
///
/// Implementation detail used by the [`renders!`] macro to wire a concrete
/// architecture renderer to the active set of codegen backends.  Its shape
/// will change as backends are migrated to the `StackOp` pipeline.
#[doc(hidden)]
pub trait ArchTarget {
    #[cfg(feature = "target-js")]
    type Js<'a>: Display;
    #[cfg(feature = "target-wasm")]
    type Wasm<'a>: WasmJit;

    #[cfg(feature = "target-js")]
    fn target_js<'a>(&self, j: &'a TemplateJit<'_>) -> Self::Js<'a>;
    #[cfg(feature = "target-wasm")]
    fn target_wasm<'a>(&self, j: &'a TemplateJit<'_>) -> Self::Wasm<'a>;
}

impl<'b> TemplateJit<'b> {
    /// Unified JIT dispatch used by all targets.
    ///
    /// On first visit to `self.pc` a named loop is opened and the body is
    /// produced by calling `go`.  On repeat visit a backward branch is emitted
    /// instead.  Both are expressed purely in terms of `CoreOpcode`, so any
    /// target whose opcode type implements that trait can use this method.
    pub fn jit_core<'a, O, I>(
        &'a self,
        go: impl 'a + FnOnce(Labels, u32) -> I,
    ) -> impl Iterator<Item = O> + 'a
    where
        O: CoreOpcode<'a>,
        I: Iterator<Item = O> + 'a,
    {
        // Collected eagerly so no `gen` syntax is needed: the unstable
        // `gen_blocks` feature gate fires at parse time, which cfg-stripping
        // cannot hide, and its presence in this file broke stable builds of
        // the interpreter-only path (spday-exec).
        let mut labels = self.labels.clone();
        let out: alloc::vec::Vec<O> = match labels.0.entry(self.pc) {
            alloc::collections::btree_map::Entry::Vacant(vacant_entry) => {
                let label_name = alloc::format!("x{}", self.pc);
                vacant_entry.insert(Label {
                    ident_name: label_name.clone(),
                    index: self.depth,
                });
                let nd = self.depth + 1;
                let i = go(labels, nd);
                O::r#loop(i, label_name).collect()
            }
            alloc::collections::btree_map::Entry::Occupied(occupied_entry) => {
                let Label { index, ident_name } = occupied_entry.get().clone();
                let relative_depth = self.depth - index;
                O::br(relative_depth, ident_name).collect()
            }
        };
        out.into_iter()
    }
}

// ── Stack IR renderers ───────────────────────────────────────────────────────

/// Render a `StackOp` stream to JavaScript.
///
/// Maintains a temporary-variable stack (names `_t0`, `_t1`, …) and a label
/// stack for `LoopBegin`/`LoopEnd` matching.  Every op either writes a JS
/// statement to `f` directly or pushes an expression name for a later op to
/// consume.
///
/// `flags.async_mem` (see [`Flags`]) switches `LoadReg`/`StoreReg`/
/// `LoadMem`/`StoreMem` from the default inline `DataView`/`$.r` expressions
/// to `await $.reg_read`/`$.reg_write`/`$.mem_read`/`$.mem_write` calls. It
/// defaults to `false`, so existing callers see byte-identical output.
#[cfg(feature = "target-js")]
pub fn render_stack_to_js_with_names<N: HostMethodNames<VaneHostMethod>>(
    ops: impl Iterator<Item = StackOp>,
    f: &mut Formatter<'_>,
    flate: &(dyn crate::Flate + '_),
    flags: Flags,
    names: &N,
) -> core::fmt::Result {
    validate_host_method_names(names).map_err(|_| core::fmt::Error)?;
    let registers = host_member(names, VaneHostMethod::Registers);
    let register_cache = host_member(names, VaneHostMethod::RegisterCache);
    let jit_cache = host_member(names, VaneHostMethod::JitCache);
    let reg_read = host_member(names, VaneHostMethod::RegRead);
    let reg_write = host_member(names, VaneHostMethod::RegWrite);
    let mem_read = host_member(names, VaneHostMethod::MemRead);
    let mem_write = host_member(names, VaneHostMethod::MemWrite);
    let ecall = host_member(names, VaneHostMethod::Ecall);
    let async_mem = flags.async_mem;
    let max64 = alloc::format!("{}", flate.flate("max64"));
    let max32 = alloc::format!("{}", flate.flate("max32"));
    let signed = alloc::format!("{}", flate.flate("signed"));
    let unsigned = alloc::format!("{}", flate.flate("unsigned"));

    let data = alloc::format!("{}", flate.flate("data"));
    let mut val: alloc::vec::Vec<alloc::string::String> = alloc::vec::Vec::new();
    let mut lbls: alloc::vec::Vec<alloc::string::String> = alloc::vec::Vec::new();
    let mut tmp = 0usize;

    macro_rules! push {
        ($expr:expr) => {{
            let name = alloc::format!("_t{tmp}");
            tmp += 1;
            write!(f, "let {name}={};", $expr)?;
            val.push(name);
        }};
    }
    macro_rules! pop {
        () => {
            val.pop().unwrap_or_default()
        };
    }
    macro_rules! pop2 {
        () => {{
            let b = val.pop().unwrap_or_default();
            let a = val.pop().unwrap_or_default();
            (a, b)
        }};
    }

    for op in ops {
        match op {
            // ── Control flow ─────────────────────────────────────────────
            StackOp::LoopBegin { label } => {
                write!(f, "{label}: for(;;){{")?;
                lbls.push(label);
            }
            StackOp::LoopEnd => {
                let label = lbls.pop().unwrap_or_default();
                write!(f, "break {label};}}")?;
            }
            StackOp::Br { label, .. } => {
                write!(f, "continue {label};")?;
            }
            StackOp::IfBegin => {
                let cond = pop!();
                write!(f, "if({cond}!==0n){{")?;
            }
            StackOp::IfElse => {
                write!(f, "}}else{{")?;
            }
            StackOp::IfEnd => {
                write!(f, "}}")?;
            }

            // ── Literals / registers ──────────────────────────────────────
            StackOp::PushImm(v) => {
                push!(alloc::format!("{v}n"));
            }
            StackOp::LoadReg(r) => {
                if r == 0 {
                    push!("0n");
                } else if async_mem {
                    push!(alloc::format!("(await {reg_read}({r}))"));
                } else {
                    push!(alloc::format!(
                        "(({register_cache}??={registers})[`x{r}`]??=0n)"
                    ));
                }
            }
            StackOp::StoreReg(r) => {
                let v = pop!();
                if r != 0 && async_mem {
                    write!(f, "await {reg_write}({r},{v});")?;
                } else if r != 0 {
                    write!(f, "(({register_cache}??={registers})[`x{r}`]={v});")?;
                }
                // x0 is hardwired zero — discard silently.
            }
            StackOp::Drop => {
                let _ = pop!();
            }
            StackOp::LoadState(slot) => {
                let slot = js_string_literal(slot.as_str());
                push!(alloc::format!(
                    "(({register_cache}??={registers})[{slot}]??=0n)"
                ));
            }
            StackOp::StoreState(slot) => {
                let value = pop!();
                let slot = js_string_literal(slot.as_str());
                write!(f, "(({register_cache}??={registers})[{slot}]={value});")?;
            }

            // ── 64-bit arithmetic ─────────────────────────────────────────
            StackOp::Add64 => {
                let (a, b) = pop2!();
                push!(alloc::format!("({a}+{b})&{max64}"));
            }
            StackOp::Sub64 => {
                let (a, b) = pop2!();
                push!(alloc::format!("({a}-{b})&{max64}"));
            }
            StackOp::Mul64 => {
                let (a, b) = pop2!();
                push!(alloc::format!("({a}*{b})&{max64}"));
            }
            StackOp::DivU64 => {
                let (a, b) = pop2!();
                push!(alloc::format!("{b}===0n?{unsigned}(-1n):({a}/{b})&{max64}"));
            }
            StackOp::DivS64 => {
                let (a, b) = pop2!();
                push!(alloc::format!(
                    "{b}===0n?{unsigned}(-1n):{unsigned}({signed}({a})/{signed}({b}))&{max64}"
                ));
            }
            StackOp::RemU64 => {
                let (a, b) = pop2!();
                // RISC-V M extension: remainder-by-zero is the dividend.
                push!(alloc::format!("{b}===0n?({a}):({a}%{b})&{max64}"));
            }
            StackOp::RemS64 => {
                let (a, b) = pop2!();
                push!(alloc::format!(
                    "{b}===0n?({a}):{unsigned}({signed}({a})%{signed}({b}))&{max64}"
                ));
            }
            StackOp::And64 => {
                let (a, b) = pop2!();
                push!(alloc::format!("({a}&{b})&{max64}"));
            }
            StackOp::Or64 => {
                let (a, b) = pop2!();
                push!(alloc::format!("({a}|{b})&{max64}"));
            }
            StackOp::Xor64 => {
                let (a, b) = pop2!();
                push!(alloc::format!("({a}^{b})&{max64}"));
            }
            StackOp::Shl64 => {
                let (a, b) = pop2!();
                push!(alloc::format!("({a}<<{b})&{max64}"));
            }
            StackOp::ShrU64 => {
                let (a, b) = pop2!();
                push!(alloc::format!("({a}>>{b})&{max64}"));
            }
            StackOp::ShrS64 => {
                let (a, b) = pop2!();
                push!(alloc::format!(
                    "{unsigned}({signed}({a})>>{signed}({b}))&{max64}"
                ));
            }
            StackOp::MulHU64 => {
                let (a, b) = pop2!();
                push!(alloc::format!("(({a}*{b})>>64n)&{max64}"));
            }
            StackOp::MulHSU64 => {
                let (a, b) = pop2!();
                push!(alloc::format!(
                    "{unsigned}(({signed}({a})*{b})>>64n)&{max64}"
                ));
            }
            StackOp::MulHS64 => {
                let (a, b) = pop2!();
                push!(alloc::format!(
                    "{unsigned}(({signed}({a})*{signed}({b}))>>64n)&{max64}"
                ));
            }

            // ── 32-bit arithmetic (result sign-extended to i64) ───────────
            StackOp::Add32 => {
                let (a, b) = pop2!();
                push!(alloc::format!("{unsigned}(BigInt.asIntN(32,{a}+{b}))"));
            }
            StackOp::Sub32 => {
                let (a, b) = pop2!();
                push!(alloc::format!("{unsigned}(BigInt.asIntN(32,{a}-{b}))"));
            }
            StackOp::Mul32 => {
                let (a, b) = pop2!();
                push!(alloc::format!(
                    "{unsigned}(BigInt.asIntN(32,({a}&{max32})*({b}&{max32})))"
                ));
            }
            StackOp::DivU32 => {
                let (a, b) = pop2!();
                push!(alloc::format!(
                    "{b}===0n?{unsigned}(-1n):{unsigned}(BigInt.asIntN(32,({a}&{max32})/({b}&{max32})))"
                ));
            }
            StackOp::DivS32 => {
                let (a, b) = pop2!();
                push!(alloc::format!(
                    "{b}===0n?{unsigned}(-1n):{unsigned}(BigInt.asIntN(32,{signed}({a}&{max32})/{signed}({b}&{max32})))"
                ));
            }
            StackOp::RemU32 => {
                let (a, b) = pop2!();
                push!(alloc::format!(
                    // RISC-V M extension: remainder-by-zero is the dividend.
                    "{b}===0n?{unsigned}(BigInt.asIntN(32,({a}&{max32}))):{unsigned}(BigInt.asIntN(32,({a}&{max32})%({b}&{max32})))"
                ));
            }
            StackOp::RemS32 => {
                let (a, b) = pop2!();
                push!(alloc::format!(
                    // RISC-V M extension: remainder-by-zero is the dividend.
                    "{b}===0n?{unsigned}({signed}({a}&{max32})):{unsigned}(BigInt.asIntN(32,{signed}({a}&{max32})%{signed}({b}&{max32})))"
                ));
            }
            StackOp::Shl32 => {
                let (a, b) = pop2!();
                push!(alloc::format!(
                    "{unsigned}(BigInt.asIntN(32,({a}&{max32})<<({b}&{max32})))"
                ));
            }
            StackOp::ShrU32 => {
                let (a, b) = pop2!();
                push!(alloc::format!(
                    "{unsigned}(BigInt.asIntN(32,({a}&{max32})>>({b}&{max32})))"
                ));
            }
            StackOp::ShrS32 => {
                let (a, b) = pop2!();
                push!(alloc::format!(
                    "{unsigned}(BigInt.asIntN(32,{signed}({a}&{max32})>>{signed}({b}&{max32})))"
                ));
            }

            // ── Conversions ───────────────────────────────────────────────
            StackOp::Trunc32 => {
                let a = pop!();
                push!(alloc::format!("({a}&{max32})"));
            }
            StackOp::SignExt32 => {
                let a = pop!();
                push!(alloc::format!("{unsigned}(BigInt.asIntN(32,{a}))"));
            }
            StackOp::AsSigned64 => {
                let a = pop!();
                push!(alloc::format!("{signed}({a})"));
            }
            StackOp::AsUnsigned64 => {
                let a = pop!();
                push!(alloc::format!("{unsigned}({a})"));
            }

            // ── Comparisons (push 1n or 0n) ───────────────────────────────
            StackOp::Eq64 => {
                let (a, b) = pop2!();
                push!(alloc::format!("({a}==={b}?1n:0n)"));
            }
            StackOp::Ne64 => {
                let (a, b) = pop2!();
                push!(alloc::format!("({a}!=={b}?1n:0n)"));
            }
            StackOp::LtU64 => {
                let (a, b) = pop2!();
                push!(alloc::format!("({a}<{b}?1n:0n)"));
            }
            StackOp::LtS64 => {
                let (a, b) = pop2!();
                push!(alloc::format!("({signed}({a})<{signed}({b})?1n:0n)"));
            }
            StackOp::GeU64 => {
                let (a, b) = pop2!();
                push!(alloc::format!("({a}>={b}?1n:0n)"));
            }
            StackOp::GeS64 => {
                let (a, b) = pop2!();
                push!(alloc::format!("({signed}({a})>={signed}({b})?1n:0n)"));
            }
            StackOp::FloatUnary { width, op } => {
                let a = pop!();
                let width = match width {
                    FloatWidth::F32 => "32",
                    FloatWidth::F64 => "64",
                };
                let op = match op {
                    FloatUnaryOp::Abs => "abs",
                    FloatUnaryOp::Neg => "neg",
                    FloatUnaryOp::Sqrt => "sqrt",
                };
                push!(alloc::format!("fp_unary({width},\"{op}\",{a})"));
            }
            StackOp::FloatBinary { width, op } => {
                let (a, b) = pop2!();
                let width = match width {
                    FloatWidth::F32 => "32",
                    FloatWidth::F64 => "64",
                };
                let op = match op {
                    FloatBinOp::Add => "add",
                    FloatBinOp::Sub => "sub",
                    FloatBinOp::Mul => "mul",
                    FloatBinOp::Div => "div",
                    FloatBinOp::Min => "min",
                    FloatBinOp::Max => "max",
                };
                push!(alloc::format!("fp_binary({width},\"{op}\",{a},{b})"));
            }
            StackOp::FloatLt { width } => {
                let (a, b) = pop2!();
                let width = match width {
                    FloatWidth::F32 => "32",
                    FloatWidth::F64 => "64",
                };
                push!(alloc::format!("fp_lt({width},{a},{b})?1n:0n"));
            }
            StackOp::FloatEq { width } => {
                let (a, b) = pop2!();
                let width = match width {
                    FloatWidth::F32 => "32",
                    FloatWidth::F64 => "64",
                };
                push!(alloc::format!("fp_eq({width},{a},{b})?1n:0n"));
            }
            StackOp::FloatUnordered { width } => {
                let (a, b) = pop2!();
                let width = match width {
                    FloatWidth::F32 => "32",
                    FloatWidth::F64 => "64",
                };
                push!(alloc::format!("fp_unordered({width},{a},{b})?1n:0n"));
            }
            StackOp::IntToFloat {
                width,
                signed: is_signed,
                int_width,
            } => {
                let value = pop!();
                let width = match width {
                    FloatWidth::F32 => "32",
                    FloatWidth::F64 => "64",
                };
                let signed = if is_signed { "true" } else { "false" };
                push!(alloc::format!(
                    "fp_from_int({width},{signed},{int_width},{value})"
                ));
            }
            StackOp::FloatToInt {
                width,
                signed: is_signed,
                int_width,
            } => {
                let value = pop!();
                let width = match width {
                    FloatWidth::F32 => "32",
                    FloatWidth::F64 => "64",
                };
                let signed = if is_signed { "true" } else { "false" };
                push!(alloc::format!(
                    "fp_to_int({width},{signed},{int_width},{value})"
                ));
            }
            StackOp::FloatConvert { from, to } => {
                let value = pop!();
                let from = match from {
                    FloatWidth::F32 => "32",
                    FloatWidth::F64 => "64",
                };
                let to = match to {
                    FloatWidth::F32 => "32",
                    FloatWidth::F64 => "64",
                };
                push!(alloc::format!("fp_convert({from},{to},{value})"));
            }

            // ── Memory ────────────────────────────────────────────────────
            StackOp::LoadMem {
                width,
                signed: is_signed,
            } => {
                let addr = pop!();
                if async_mem {
                    let width_bytes = match width {
                        MemWidth::B1 => 1,
                        MemWidth::B2 => 2,
                        MemWidth::B4 => 4,
                        MemWidth::B8 => 8,
                    };
                    let is_signed = if is_signed { "true" } else { "false" };
                    push!(alloc::format!(
                        "(await {mem_read}(({addr})&{max64},{width_bytes},{is_signed}))"
                    ));
                } else {
                    let expr = match (width, is_signed) {
                        (MemWidth::B1, true) => alloc::format!(
                            "{unsigned}(BigInt({data}(({addr})&{max64}).getInt8(0,true)))"
                        ),
                        (MemWidth::B1, false) => {
                            alloc::format!("BigInt({data}(({addr})&{max64}).getUint8(0,true))")
                        }
                        (MemWidth::B2, true) => alloc::format!(
                            "{unsigned}(BigInt({data}(({addr})&{max64}).getInt16(0,true)))"
                        ),
                        (MemWidth::B2, false) => {
                            alloc::format!("BigInt({data}(({addr})&{max64}).getUint16(0,true))")
                        }
                        (MemWidth::B4, true) => alloc::format!(
                            "{unsigned}(BigInt({data}(({addr})&{max64}).getInt32(0,true)))"
                        ),
                        (MemWidth::B4, false) => {
                            alloc::format!("BigInt({data}(({addr})&{max64}).getUint32(0,true))")
                        }
                        (MemWidth::B8, _) => {
                            alloc::format!("{data}(({addr})&{max64}).getBigUint64(0,true)")
                        }
                    };
                    push!(expr);
                }
            }
            StackOp::StoreMem { width } => {
                let v = pop!();
                let addr = pop!();
                if async_mem {
                    let width_bytes = match width {
                        MemWidth::B1 => 1,
                        MemWidth::B2 => 2,
                        MemWidth::B4 => 4,
                        MemWidth::B8 => 8,
                    };
                    write!(f, "await {mem_write}(({addr})&{max64},{width_bytes},{v});")?;
                } else {
                    match width {
                        MemWidth::B1 => write!(
                            f,
                            "{data}(({addr})&{max64}).setUint8(0,Number({v}&{max32}),true);"
                        )?,
                        MemWidth::B2 => write!(
                            f,
                            "{data}(({addr})&{max64}).setUint16(0,Number({v}&{max32}),true);"
                        )?,
                        MemWidth::B4 => write!(
                            f,
                            "{data}(({addr})&{max64}).setUint32(0,Number({v}&{max32}),true);"
                        )?,
                        MemWidth::B8 => {
                            write!(f, "{data}(({addr})&{max64}).setBigUint64(0,{v},true);")?
                        }
                    }
                }
            }

            // ── Side effects ──────────────────────────────────────────────
            StackOp::Ecall => {
                write!(
                    f,
                    "let _ecall=await {ecall}();if(_ecall?.kind==='exit')return _ecall;"
                )?;
            }
            StackOp::Trap(msg) => {
                write!(f, "throw new TypeError(`{msg}`);")?;
            }
            StackOp::TailCall => {
                let addr = pop!();
                write!(f, "return ()=>J({addr});")?;
            }
            StackOp::Log(msg) => {
                write!(f, "console.log(`{msg}`);")?;
            }

            // ── Integrity check ───────────────────────────────────────────
            StackOp::CheckCode {
                addr,
                expected,
                miss_label,
            } => {
                match expected.len() {
                    4 => {
                        let v = u32::from_le_bytes([
                            expected[0],
                            expected[1],
                            expected[2],
                            expected[3],
                        ]);
                        write!(
                            f,
                            "if({data}({addr}n).getUint32(0,true)!=={v}){{delete {jit_cache}[`{miss_label}`];return J({addr}n);}};"
                        )?
                    }
                    2 => {
                        let v = u16::from_le_bytes([expected[0], expected[1]]);
                        write!(
                            f,
                            "if({data}({addr}n).getUint16(0,true)!=={v}){{delete {jit_cache}[`{miss_label}`];return J({addr}n);}};"
                        )?
                    }
                    _ => {} // unknown width — skip check
                }
            }
        }
    }
    Ok(())
}

/// Render with Vane's canonical, zero-sized host-name scheme.
#[cfg(feature = "target-js")]
pub fn render_stack_to_js(
    ops: impl Iterator<Item = StackOp>,
    f: &mut Formatter<'_>,
    flate: &(dyn crate::Flate + '_),
    flags: Flags,
) -> core::fmt::Result {
    render_stack_to_js_with_names(ops, f, flate, flags, &CanonicalHostMethodNames)
}

// The WASM renderer uses unstable `gen`-block syntax, which a cfg attribute
// on the item cannot hide from the parser (files are parsed whole). Gating
// the *module file* instead keeps interpreter-only builds (no target-wasm)
// parseable by stable toolchains.
#[cfg(feature = "target-wasm")]
mod render_wasm;
#[cfg(feature = "target-wasm")]
pub use render_wasm::render_stack_to_wasm;

/// Core JavaScript code generator with paging support.
///
/// Generates the outer JS wrapper that declares the arithmetic helpers and the
/// `data` memory-access function for a JIT-compiled block.
///
/// # Paging in JavaScript
/// The generated `data` function performs address translation based on paging_mode:
/// - Legacy: `data = (p => { p = $.get_page(p); return new DataView(...); })`
/// - Shared/Both: Uses inline page table translation
///
/// See PAGING.md for detailed documentation on the paging system.
#[cfg(feature = "target-js")]
pub struct CoreJS<'a> {
    pub content: &'a (dyn Display + 'a),
    pub flate: &'a (dyn Flate + 'a),
    pub flags: Flags,
}

#[cfg(feature = "target-js")]
pub struct CoreJSWithNames<'a, N> {
    pub content: &'a (dyn Display + 'a),
    pub flate: &'a (dyn Flate + 'a),
    pub flags: Flags,
    pub names: N,
}

/// A name-configurable wrapper around [`CoreJS`].  It resolves all `$` ABI
/// accesses while compiling the wrapper; the generated function retains only
/// direct or safely escaped member expressions.
#[cfg(feature = "target-js")]
impl<'a, N: HostMethodNames<VaneHostMethod>> Display for CoreJSWithNames<'a, N> {
    fn fmt(&self, fmt: &mut Formatter<'_>) -> core::fmt::Result {
        validate_host_method_names(&self.names).map_err(|_| core::fmt::Error)?;
        let canonical = alloc::format!(
            "{}",
            CoreJS {
                content: self.content,
                flate: self.flate,
                flags: self.flags,
            }
        );
        let source = rewrite_core_js_host_members(&canonical, &self.names);
        fmt.write_str(&source)
    }
}

#[cfg(feature = "target-js")]
fn rewrite_core_js_host_members<N: HostMethodNames<VaneHostMethod>>(
    source: &str,
    names: &N,
) -> alloc::string::String {
    // One left-to-right pass is essential: a mapped spelling may itself equal
    // a different canonical spelling and must not be rewritten twice.
    let replacements = [
        ("$._sys", VaneHostMethod::System),
        ("$.get_page", VaneHostMethod::GetPage),
        ("$.f", VaneHostMethod::Max64),
    ];
    let mut rest = source;
    let mut out = alloc::string::String::with_capacity(source.len());
    while !rest.is_empty() {
        let next = replacements
            .iter()
            .filter_map(|(needle, method)| rest.find(needle).map(|at| (at, *needle, *method)))
            .min_by_key(|(at, _, _)| *at);
        let Some((at, needle, method)) = next else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..at]);
        out.push_str(&host_member(names, method));
        rest = &rest[at + needle.len()..];
    }
    out
}

#[cfg(feature = "target-js")]
impl<'a> CoreJS<'a> {
    /// Attach a custom, compile-time-resolved host-name scheme.
    pub fn with_names<N: HostMethodNames<VaneHostMethod>>(
        self,
        names: N,
    ) -> CoreJSWithNames<'a, N> {
        CoreJSWithNames {
            content: self.content,
            flate: self.flate,
            flags: self.flags,
            names,
        }
    }
}

#[cfg(feature = "target-js")]
impl<'a> Display for CoreJS<'a> {
    fn fmt(&self, fmt: &mut Formatter<'_>) -> core::fmt::Result {
        let max64 = self.flate.flate("max64");
        let max32 = self.flate.flate("max32");
        let signed = self.flate.flate("signed");
        let unsigned = self.flate.flate("unsigned");
        let data = self.flate.flate("data");

        write!(
            fmt,
            "return async function(){{let {max64}=$.f,{max32}=0xffff_ffffn,{signed}=(a=>BigInt.asIntN(64,a)),{unsigned}=(a=>BigInt.asUintN(64,a)),fp_buf=new ArrayBuffer(8),fp_dv=new DataView(fp_buf),fp_unary=((w,o,a)=>{{if(w===32){{fp_dv.setUint32(0,Number(a&0xffffffffn),true);let v=fp_dv.getFloat32(0,true);v=o===\"abs\"?Math.abs(v):o===\"neg\"?-v:Math.sqrt(v);fp_dv.setFloat32(0,v,true);return BigInt(fp_dv.getUint32(0,true));}}fp_dv.setBigUint64(0,a,true);let v=fp_dv.getFloat64(0,true);v=o===\"abs\"?Math.abs(v):o===\"neg\"?-v:Math.sqrt(v);fp_dv.setFloat64(0,v,true);return fp_dv.getBigUint64(0,true);}}),fp_binary=((w,o,a,b)=>{{let cv=(x=>{{if(w===32){{fp_dv.setUint32(0,Number(x&0xffffffffn),true);return fp_dv.getFloat32(0,true);}}fp_dv.setBigUint64(0,x,true);return fp_dv.getFloat64(0,true);}}),av=cv(a),bv=cv(b),v=o===\"add\"?av+bv:o===\"sub\"?av-bv:o===\"mul\"?av*bv:o===\"div\"?av/bv:o===\"min\"?Math.min(av,bv):Math.max(av,bv);if(w===32){{fp_dv.setFloat32(0,v,true);return BigInt(fp_dv.getUint32(0,true));}}fp_dv.setFloat64(0,v,true);return fp_dv.getBigUint64(0,true);}}),fp_lt=((w,a,b)=>{{if(w===32){{fp_dv.setUint32(0,Number(a&0xffffffffn),true);let av=fp_dv.getFloat32(0,true);fp_dv.setUint32(0,Number(b&0xffffffffn),true);return av<fp_dv.getFloat32(0,true);}}fp_dv.setBigUint64(0,a,true);let av=fp_dv.getFloat64(0,true);fp_dv.setBigUint64(0,b,true);return av<fp_dv.getFloat64(0,true);}}),fp_eq=((w,a,b)=>{{if(w===32){{fp_dv.setUint32(0,Number(a&0xffffffffn),true);let av=fp_dv.getFloat32(0,true);fp_dv.setUint32(0,Number(b&0xffffffffn),true);return av===fp_dv.getFloat32(0,true);}}fp_dv.setBigUint64(0,a,true);let av=fp_dv.getFloat64(0,true);fp_dv.setBigUint64(0,b,true);return av===fp_dv.getFloat64(0,true);}}),fp_from_int=((w,s,b,x)=>{{let n=s?(b===32?Number(BigInt.asIntN(32,x)):Number(BigInt.asIntN(64,x))):(b===32?Number(BigInt.asUintN(32,x)):Number(BigInt.asUintN(64,x)));if(w===32){{fp_dv.setFloat32(0,n,true);return BigInt(fp_dv.getUint32(0,true));}}fp_dv.setFloat64(0,n,true);return fp_dv.getBigUint64(0,true);}}),fp_to_int=((w,s,b,x)=>{{let n;if(w===32){{fp_dv.setUint32(0,Number(x&0xffffffffn),true);n=fp_dv.getFloat32(0,true);}}else{{fp_dv.setBigUint64(0,x,true);n=fp_dv.getFloat64(0,true);}}return BigInt.asUintN(b,BigInt(Math.trunc(n)));}}),fp_convert=((a,b,x)=>{{if(a===b)return x;if(a===32){{fp_dv.setUint32(0,Number(x&0xffffffffn),true);fp_dv.setFloat64(0,fp_dv.getFloat32(0,true),true);return fp_dv.getBigUint64(0,true);}}fp_dv.setBigUint64(0,x,true);fp_dv.setFloat32(0,fp_dv.getFloat64(0,true),true);return BigInt(fp_dv.getUint32(0,true));}}),fp_unordered=((w,a,b)=>{{if(w===32){{fp_dv.setUint32(0,Number(a&0xffffffffn),true);let av=fp_dv.getFloat32(0,true);fp_dv.setUint32(0,Number(b&0xffffffffn),true);return Number.isNaN(av)||Number.isNaN(fp_dv.getFloat32(0,true));}}fp_dv.setBigUint64(0,a,true);let av=fp_dv.getFloat64(0,true);fp_dv.setBigUint64(0,b,true);return Number.isNaN(av)||Number.isNaN(fp_dv.getFloat64(0,true));}}),",
        )?;

        self.write_data_function(fmt, &data)?;

        write!(fmt, ";{}}}", &self.content)
    }
}

#[cfg(feature = "target-js")]
impl<'a> CoreJS<'a> {
    fn write_read_helpers(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        writeln!(f, "    let read_u64=(addr)=>{{")?;
        writeln!(
            f,
            "        let dv=new DataView($._sys('memory').buffer,$.get_page(addr),8);"
        )?;
        writeln!(f, "        return dv.getBigUint64(0,true);")?;
        writeln!(f, "    }};")?;

        if self.flags.use_32bit_paging {
            writeln!(f, "    let read_u32=(addr)=>{{")?;
            writeln!(
                f,
                "        let dv=new DataView($._sys('memory').buffer,$.get_page(addr),4);"
            )?;
            writeln!(f, "        return dv.getUint32(0,true);")?;
            writeln!(f, "    }};")?;
        }

        Ok(())
    }

    fn write_multilevel_traversal(
        &self,
        f: &mut Formatter<'_>,
        pt_vaddr: u64,
        sd_vaddr: u64,
    ) -> core::fmt::Result {
        if self.flags.use_32bit_paging {
            writeln!(f, "    let l3_idx=(v>>48n)&0xFFFFn;")?;
            writeln!(
                f,
                "    let l2_table_vaddr=BigInt(read_u32({pt_vaddr}n+(l3_idx<<2n)));"
            )?;
            writeln!(f, "    let l2_idx=(v>>32n)&0xFFFFn;")?;
            writeln!(
                f,
                "    let l1_table_vaddr=BigInt(read_u32(l2_table_vaddr+(l2_idx<<2n)));"
            )?;
            writeln!(f, "    let l1_idx=(v>>16n)&0xFFFFn;")?;
            writeln!(
                f,
                "    let page_pointer=read_u32(l1_table_vaddr+(l1_idx<<2n));"
            )?;
            writeln!(f, "    let sec_idx=page_pointer&0xFF;")?;
            writeln!(f, "    let page_base_low24=page_pointer>>8;")?;
            writeln!(f, "    let sec_entry_addr={sd_vaddr}n+BigInt(sec_idx<<3);")?;
            writeln!(f, "    let sec_entry=read_u64(sec_entry_addr);")?;
            writeln!(f, "    let page_base_top8=sec_entry>>56n;")?;
            writeln!(
                f,
                "    let phys_page_base=BigInt((page_base_top8<<24n)|BigInt(page_base_low24));"
            )?;
        } else {
            writeln!(f, "    let l3_idx=(v>>48n)&0xFFFFn;")?;
            writeln!(
                f,
                "    let l2_table_vaddr=read_u64({pt_vaddr}n+(l3_idx<<3n));"
            )?;
            writeln!(f, "    let l2_idx=(v>>32n)&0xFFFFn;")?;
            writeln!(
                f,
                "    let l1_table_vaddr=read_u64(l2_table_vaddr+(l2_idx<<3n));"
            )?;
            writeln!(f, "    let l1_idx=(v>>16n)&0xFFFFn;")?;
            writeln!(
                f,
                "    let page_pointer=read_u64(l1_table_vaddr+(l1_idx<<3n));"
            )?;
            writeln!(f, "    let sec_idx=page_pointer&0xFFFFn;")?;
            writeln!(f, "    let page_base_low48=page_pointer>>16n;")?;
            writeln!(f, "    let sec_entry_addr={sd_vaddr}n+(sec_idx<<3n);")?;
            writeln!(f, "    let sec_entry=read_u64(sec_entry_addr);")?;
            writeln!(f, "    let page_base_top16=sec_entry>>48n;")?;
            writeln!(
                f,
                "    let phys_page_base=(page_base_top16<<48n)|page_base_low48;"
            )?;
        }
        Ok(())
    }

    fn write_single_level_lookup(
        &self,
        f: &mut Formatter<'_>,
        pt_vaddr: u64,
        sd_vaddr: u64,
    ) -> core::fmt::Result {
        writeln!(f, "    let page_num=v>>16n;")?;

        if self.flags.use_32bit_paging {
            writeln!(f, "    let entry_addr={pt_vaddr}n+(page_num<<2n);")?;
            writeln!(
                f,
                "    let dv=new DataView($._sys('memory').buffer,$.get_page(entry_addr),4);"
            )?;
            writeln!(f, "    let page_pointer=dv.getUint32(0,true);")?;
            writeln!(f, "    let sec_idx=page_pointer&0xFF;")?;
            writeln!(f, "    let page_base_low24=page_pointer>>8;")?;
            writeln!(f, "    let sec_entry_addr={sd_vaddr}n+BigInt(sec_idx<<3);")?;
            writeln!(f, "    let sec_entry=read_u64(sec_entry_addr);")?;
            writeln!(f, "    let page_base_top8=sec_entry>>56n;")?;
            writeln!(
                f,
                "    let phys_page_base=BigInt((page_base_top8<<24n)|BigInt(page_base_low24));"
            )?;
        } else {
            writeln!(f, "    let entry_addr={pt_vaddr}n+(page_num<<3n);")?;
            writeln!(f, "    let page_pointer=read_u64(entry_addr);")?;
            writeln!(f, "    let sec_idx=page_pointer&0xFFFFn;")?;
            writeln!(f, "    let page_base_low48=page_pointer>>16n;")?;
            writeln!(f, "    let sec_entry_addr={sd_vaddr}n+(sec_idx<<3n);")?;
            writeln!(f, "    let sec_entry=read_u64(sec_entry_addr);")?;
            writeln!(f, "    let page_base_top16=sec_entry>>48n;")?;
            writeln!(
                f,
                "    let phys_page_base=(page_base_top16<<48n)|page_base_low48;"
            )?;
        }
        Ok(())
    }

    fn write_data_function(
        &self,
        f: &mut Formatter<'_>,
        data_var: &dyn Display,
    ) -> core::fmt::Result {
        match self.flags.paging_mode {
            Some(PagingMode::Shared) | Some(PagingMode::Both) => {
                let pt_vaddr = self.flags.shared_page_table_vaddr.unwrap_or(0);
                let sd_vaddr = self.flags.shared_security_directory_vaddr.unwrap_or(0);

                writeln!(f, "{data_var}=(v=>{{")?;
                self.write_read_helpers(f)?;

                if self.flags.use_multilevel_paging {
                    self.write_multilevel_traversal(f, pt_vaddr, sd_vaddr)?;
                } else {
                    self.write_single_level_lookup(f, pt_vaddr, sd_vaddr)?;
                }

                writeln!(f, "    let p=phys_page_base+(v&0xFFFFn);")?;
                writeln!(
                    f,
                    "    return new DataView($._sys(`memory`).buffer,$.get_page(p));"
                )?;
                write!(f, "}})")
            }
            _ => {
                write!(
                    f,
                    "{data_var}=(p=>{{p=$.get_page(p);return new DataView($._sys(`memory`).buffer,p);}})"
                )
            }
        }
    }
}

#[cfg(all(test, feature = "target-js"))]
mod async_mem_tests {
    use super::*;
    use core::cell::RefCell;
    use vane_target_core::{MemWidth, StackOp, StateSlot};

    /// Renders `ops` via `render_stack_to_js` and returns the resulting JS
    /// text. `Formatter` has no public constructor, so this goes through a
    /// small `Display` wrapper — the standard way to reach one in a test.
    fn render(ops: alloc::vec::Vec<StackOp>, flags: Flags) -> alloc::string::String {
        struct W(RefCell<alloc::vec::Vec<StackOp>>, Flags);
        impl Display for W {
            fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
                let flate = crate::DebugFlate::default();
                render_stack_to_js(self.0.borrow_mut().drain(..), f, &flate, self.1)
            }
        }
        alloc::format!("{}", W(RefCell::new(ops), flags))
    }

    /// A representative mix: a couple of register loads/stores, one memory
    /// load and store, and a plain arithmetic op that never touches
    /// `async_mem`.
    fn sample_ops() -> alloc::vec::Vec<StackOp> {
        alloc::vec![
            StackOp::LoadReg(1),
            StackOp::LoadReg(2),
            StackOp::Add64,
            StackOp::StoreReg(3),
            StackOp::LoadReg(4),
            StackOp::LoadMem {
                width: MemWidth::B8,
                signed: false
            },
            StackOp::LoadReg(5),
            StackOp::StoreMem {
                width: MemWidth::B4
            },
            StackOp::LoadReg(0),
            StackOp::StoreReg(0),
        ]
    }

    #[test]
    fn async_mem_off_matches_existing_sync_codegen() {
        let out = render(sample_ops(), Flags::default());
        assert!(
            out.contains("$._r??=$.r"),
            "sync register access should be untouched: {out}"
        );
        assert!(
            out.contains(".getBigUint64(0,true)"),
            "sync mem load should be untouched: {out}"
        );
        assert!(
            out.contains(".setUint32(0,"),
            "sync mem store should be untouched: {out}"
        );
        assert!(!out.contains("await $.reg_read"));
        assert!(!out.contains("await $.mem_read"));
    }

    #[test]
    fn named_state_slots_are_escaped_and_do_not_use_riscv_x0() {
        let out = render(
            alloc::vec![
                StackOp::PushImm(9),
                StackOp::StoreState(StateSlot::new("a64:x0")),
                StackOp::LoadState(StateSlot::new("a64:x0")),
                StackOp::Drop,
            ],
            Flags::default(),
        );
        assert!(out.contains("[\"a64:x0\"]"), "state key missing: {out}");
        assert!(!out.contains("[`x0`]"), "state key used RV64 x0: {out}");
    }

    #[test]
    fn async_mem_on_awaits_reg_and_mem_ops_only() {
        let flags = Flags {
            async_mem: true,
            ..Default::default()
        };
        let out = render(sample_ops(), flags);
        assert!(out.contains("await $.reg_read(1)"));
        assert!(out.contains("await $.reg_read(2)"));
        assert!(out.contains("await $.reg_write(3,"));
        assert!(out.contains("await $.mem_read("));
        assert!(out.contains("await $.mem_write("));
        // x0 loads/stores are compile-time constants — never worth awaiting.
        assert!(!out.contains("$.reg_read(0)"));
        assert!(!out.contains("$.reg_write(0,"));
    }

    #[test]
    fn custom_names_are_resolved_into_async_memory_source() {
        use portal_jit_host_names::MappedHostMethodNames;

        let names = MappedHostMethodNames::new([
            ("r".into(), "a".into()),
            ("_r".into(), "b".into()),
            ("p".into(), "c".into()),
            ("f".into(), "d".into()),
            ("get_page".into(), "e".into()),
            ("_sys".into(), "f".into()),
            ("ecall".into(), "g".into()),
            ("reg_read".into(), "not-a-name".into()),
            ("reg_write".into(), "i".into()),
            ("mem_read".into(), "j".into()),
            ("mem_write".into(), "k".into()),
        ]);
        struct W(
            RefCell<alloc::vec::Vec<StackOp>>,
            Flags,
            MappedHostMethodNames,
        );
        impl Display for W {
            fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
                let flate = crate::DebugFlate::default();
                render_stack_to_js_with_names(
                    self.0.borrow_mut().drain(..),
                    f,
                    &flate,
                    self.1,
                    &self.2,
                )
            }
        }
        let out = alloc::format!(
            "{}",
            W(
                RefCell::new(alloc::vec![StackOp::LoadReg(1), StackOp::Ecall]),
                Flags {
                    async_mem: true,
                    ..Default::default()
                },
                names,
            )
        );
        assert!(out.contains("await $[\"not-a-name\"](1)"), "{out}");
        assert!(out.contains("await $.g()"), "{out}");
    }

    #[test]
    fn async_mem_flag_leaves_non_mem_ops_byte_identical() {
        // Ops that never touch registers/memory should render identically
        // regardless of async_mem — only LoadReg/StoreReg/LoadMem/StoreMem
        // sites are allowed to change shape.
        let arith = || {
            alloc::vec![
                StackOp::PushImm(1),
                StackOp::PushImm(2),
                StackOp::Add64,
                StackOp::Trunc32
            ]
        };
        let off = render(arith(), Flags::default());
        let on = render(
            arith(),
            Flags {
                async_mem: true,
                ..Default::default()
            },
        );
        assert_eq!(off, on);
    }
}
