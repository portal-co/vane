# Code Printing Refactor: `CoreOpcode`-Unified Codegen

## Status: Plan (pending review)

---

## 1. Problem Statement

The JIT code-printing system currently has **three separate pipelines** that share almost no
structure:

| Pipeline | Location | Output mechanism |
|---|---|---|
| JS codegen | `vane-arch/src/template.rs` `jit_js` | Direct writes to `core::fmt::Formatter` |
| WASM JIT | `vane-arch/src/template.rs` `jit_wasm` | `Iterator<Item = JitOpcode>` |
| `CoreOpcode` path | `vane-arch/src/template.rs` `jit_core` | `Iterator<Item = O: CoreOpcode>` — **not used in production** |

The three pipelines duplicate the same control-flow logic (loop insertion on first visit, branch on
repeat visit via a `Labels` map). The `CoreOpcode` trait exists in `vane-target-core` specifically
to abstract this, but it is wired to nothing — `jit_core` is dead code today.

Additionally the old `vane-jit` crate contains a third full copy of the same templates
(`template.rs`, `arch.rs`, `template/riscv.rs`) now reduced to a single-line
`pub use vane_riscv::*` shim.

**The goal:** introduce a small stack-based IR (`StackOp`) as the single output type for all
arch renderers. `StackOp` implements `CoreOpcode`. Each compilation target (JS, WASM, native) is
a *shim pass* — a small iterator adapter — that lowers `StackOp` to target output. The arch
code is written once and knows nothing about any target.

```
TemplateJit::jit_core  ─→  Iterator<StackOp>  ─→  JsShim   ─→  JS string
                                               └─→  WasmShim ─→  wasmparser::Operator stream
                                               └─→  (future) NativeShim ─→  Cranelift IR
```

No backwards compatibility is preserved; dead code is deleted.

---

## 2. Resolved Decisions

| # | Decision |
|---|---|
| gen-blocks | Rely on nightly generator blocks throughout; no stable Vec-collect fallback |
| Lazy `If` arms | `StackOp::If` holds `Box<dyn Iterator<Item = StackOp>>` for each arm — never pre-collected |
| `CheckCode` bytes | Takes `Box<[u8]>` (arbitrary byte sequence) so any arch can use it, not just 4-byte RISC-V words |
| `vane-jit` crate | Delete from workspace entirely |
| WASM registers | Map to WASM function **locals/parameters**, not globals — avoids emitting global declarations and stays compatible with the existing JS host interface |
| Native / Cranelift | Deferred; `StackOp` is already the right shape |

---

## 3. Current `CoreOpcode` Bug

```rust
// vane-target-core/src/lib.rs — today
pub trait CoreOpcode {
    fn r#loop(i: impl Iterator<Item = Self>, name: impl Display) -> impl Iterator<Item = Self>;
    fn br(index: u32, name: impl Display) -> impl Iterator<Item = Self>;
}
```

`jit_core` calls `O::br(*index, name)` where `index` is the *absolute* label depth stored in
the `Labels` map. WASM `Br` needs a *relative* depth = `current_depth − label_depth`. The
working `jit_wasm` computes this correctly; `jit_core` does not.

**Fix:** rename the parameter to `relative_depth` and update `jit_core` to pass
`self.depth - label.index`.

---

## 4. The Shared IR: `StackOp`

`StackOp` is a stack-based IR defined in `vane-target-core` (`no_std + alloc`, no other deps).
Structural variants (`Loop`, `If`) are **fully lazy** — they hold `Box<dyn Iterator<Item =
StackOp>>` rather than pre-collected `Vec`s. This keeps the arch renderer a pure lazy stream
with no internal allocation beyond what the gen-block machinery requires.

```rust
// vane-target-core/src/stack.rs

#[derive(Clone, Copy, Debug)]
pub enum MemWidth { B1, B2, B4, B8 }

pub enum StackOp {
    // ── Control flow ──────────────────────────────────────────────────────
    /// Named infinite loop. Body is consumed exactly once by the shim.
    Loop {
        label: alloc::string::String,
        body:  alloc::boxed::Box<dyn Iterator<Item = StackOp>>,
    },
    /// Unconditional backward branch.
    /// `relative_depth` counts open loops outward from the branch site.
    /// `label` is the human-readable name used by the JS shim (`continue label`).
    Br { relative_depth: u32, label: alloc::string::String },
    /// Conditional split: pop top-of-stack (u64; 0 = false), take one arm.
    /// Arms are lazy and consumed once by the shim.
    If {
        then_ops: alloc::boxed::Box<dyn Iterator<Item = StackOp>>,
        else_ops: alloc::boxed::Box<dyn Iterator<Item = StackOp>>,
    },

    // ── Stack literals ────────────────────────────────────────────────────
    PushImm(u64),

    // ── Register file ─────────────────────────────────────────────────────
    /// Push value of architectural register `r`. x0 always pushes 0.
    LoadReg(u8),
    /// Pop and write to register `r`. x0 discards (but still evaluates the expr).
    StoreReg(u8),

    // ── 64-bit integer arithmetic ─────────────────────────────────────────
    Add64, Sub64, Mul64,
    DivU64, DivS64, RemU64, RemS64,
    And64, Or64, Xor64,
    Shl64, ShrU64, ShrS64,
    MulHU64,   // high 64 bits of unsigned 64×64
    MulHSU64,  // high 64 bits of signed×unsigned 64×64
    MulHS64,   // high 64 bits of signed 64×64

    // ── 32-bit arithmetic (result sign-extended to i64, stored as u64) ────
    Add32, Sub32, Mul32,
    DivU32, DivS32, RemU32, RemS32,
    Shl32, ShrU32, ShrS32,

    // ── Conversions ───────────────────────────────────────────────────────
    Trunc32,      // keep low 32 bits
    SignExt32,    // sign-extend bits [31:0] to 64
    AsSigned64,   // reinterpret u64 bit-pattern as i64 (BigInt.asIntN / WASM nop)
    AsUnsigned64, // reinterpret i64 bit-pattern as u64 (BigInt.asUintN / WASM nop)

    // ── Comparisons (push 1u64 or 0u64) ──────────────────────────────────
    Eq64, Ne64, LtU64, LtS64, GeU64, GeS64,

    // ── Memory ────────────────────────────────────────────────────────────
    /// Pop addr, push loaded value. `signed` controls sign-extension.
    LoadMem  { width: MemWidth, signed: bool },
    /// Pop value then addr; store.
    StoreMem { width: MemWidth },

    // ── Side effects ──────────────────────────────────────────────────────
    Ecall,
    Trap(alloc::string::String),

    // ── Instruction integrity check ───────────────────────────────────────
    /// Verify that the bytes at `addr` match `expected`.
    /// If not, invalidate the JIT cache entry for `miss_label` and exit.
    /// `expected` is arch-width-agnostic: pass 4 bytes for RISC-V, 1–15 for x86, etc.
    CheckCode {
        addr:       u64,
        expected:   alloc::boxed::Box<[u8]>,
        miss_label: alloc::string::String,
    },
}

impl CoreOpcode for StackOp {
    fn r#loop(body: impl Iterator<Item = Self>, name: impl Display) -> impl Iterator<Item = Self> {
        core::iter::once(StackOp::Loop {
            label: alloc::format!("{name}"),
            body:  alloc::boxed::Box::new(body),
        })
    }

    fn br(relative_depth: u32, name: impl Display) -> impl Iterator<Item = Self> {
        core::iter::once(StackOp::Br {
            relative_depth,
            label: alloc::format!("{name}"),
        })
    }
}
```

Because `Loop::body` and `If::then_ops`/`else_ops` are boxed iterators, `StackOp` is not
`Clone` or `Debug`. That is intentional and acceptable — the stream is single-use by design.

---

## 5. Unified `jit_core` in `TemplateJit`

`jit_core` becomes the **only** JIT dispatch method. It requires nightly `gen_blocks`.

```rust
// vane-arch/src/template.rs  (#![feature(gen_blocks)] already gated)
impl<'b> TemplateJit<'b> {
    pub fn jit_core<'a, O, I>(
        &'a self,
        go: impl 'a + for<'c> FnOnce(Labels<'c>, u32) -> I,
    ) -> impl Iterator<Item = O> + 'a
    where
        O: CoreOpcode + 'a,
        I: Iterator<Item = O> + 'a,
    {
        gen move {
            let mut labels = self.labels.clone();
            match labels.0.entry(self.pc) {
                Vacant(e) => {
                    let name = alloc::format!("x{}", self.pc);
                    e.insert(Label { ident_name: &name, index: self.depth });
                    let i = go(labels, self.depth + 1);
                    for op in O::r#loop(i, &name) { yield op; }
                }
                Occupied(e) => {
                    let Label { index, ident_name } = e.get();
                    // Fix: relative depth, not absolute index
                    for op in O::br(self.depth - index, &**ident_name) { yield op; }
                }
            }
        }
    }
}
```

`jit_js` and `jit_wasm` are **deleted**.

---

## 6. Arch Trait: One Method, One Output Type

### 6.1 `ArchTarget`

```rust
// vane-arch/src/arch.rs
pub trait ArchTarget {
    fn render<'a>(&'a self, j: &'a TemplateJit<'_>) -> impl Iterator<Item = StackOp> + 'a;
}
```

No target-specific type parameters. Targets are shims over the output iterator.

### 6.2 `renders!` macro

The macro currently generates `*Display`, `*WasmJit`, `*WasmJitDyn` traits and an
`impl TemplateJS`. All replaced by a single generated arch trait:

```rust
renders!(Riscv);

// expands to:
pub trait RiscvArch {
    fn Riscv<'a>(&'a self, j: &'a TemplateJit<'_>) -> impl Iterator<Item = StackOp> + 'a;
}
pub struct TemplateRiscv;

impl ArchTarget for TemplateRiscv {
    fn render<'a>(&'a self, j: &'a TemplateJit<'_>) -> impl Iterator<Item = StackOp> + 'a {
        j.Riscv(j)
    }
}
```

The wrapper newtype `Riscv<'a, T>` and all `Display` impls generated by `renders!` are
removed. JS formatting is the shim's responsibility.

---

## 7. RISC-V Rendering (`vane-riscv`)

### 7.1 What goes away

| Item | Replaced by |
|---|---|
| `impl RiscvDisplay for TemplateJit` | `impl RiscvArch for TemplateJit` emitting `StackOp` |
| `impl RiscvWasmJit for TemplateJit` | (deleted — WASM shim handles it) |
| `fn rv_core_js(…, f: &mut Formatter)` | `fn rv_core(…) -> impl Iterator<Item = StackOp>` |
| All `write!(f, "…")` calls | `yield StackOp::*` in a gen block |
| `TemplateReg` struct | (deleted — JS register formatting moves to `JsShim`) |
| `Flate` in `Params` | (deleted — JS-only concern, moved to `JsShim`) |

### 7.2 Instruction mappings (representative subset)

**`Add { dest, src1, src2 }`**
```rust
yield StackOp::LoadReg(src1.0);
yield StackOp::LoadReg(src2.0);
yield StackOp::Add64;
yield StackOp::StoreReg(dest.0);
```

**`Addi { dest, src1, imm }`**
```rust
yield StackOp::LoadReg(src1.0);
yield StackOp::PushImm(imm.as_i64() as u64);
yield StackOp::Add64;
yield StackOp::StoreReg(dest.0);
```

**`AddW { dest, src1, src2 }`** (result sign-extended to 64)
```rust
yield StackOp::LoadReg(src1.0);
yield StackOp::LoadReg(src2.0);
yield StackOp::Add32;
yield StackOp::StoreReg(dest.0);
```

**`Lb { dest, base, offset }`** (load byte, sign-extend)
```rust
yield StackOp::LoadReg(base.0);
yield StackOp::PushImm(offset.as_i64() as u64);
yield StackOp::Add64;
yield StackOp::LoadMem { width: MemWidth::B1, signed: true };
yield StackOp::StoreReg(dest.0);
```

**`Sd { base, src, offset }`** (store doubleword)
```rust
yield StackOp::LoadReg(base.0);
yield StackOp::PushImm(offset.as_i64() as u64);
yield StackOp::Add64;
yield StackOp::LoadReg(src.0);
yield StackOp::StoreMem { width: MemWidth::B8 };
```

**`Beq { src1, src2, offset }`** — branch arms are lazy gen blocks
```rust
let then_pc = self.pc.wrapping_add_signed(offset.as_i64() * 2);
let then_jit = TemplateJit { pc: then_pc, .. };
let else_jit = TemplateJit { pc: next_pc, .. };

yield StackOp::LoadReg(src1.0);
yield StackOp::LoadReg(src2.0);
yield StackOp::Eq64;
yield StackOp::If {
    then_ops: Box::new(then_jit.jit_core(|labels, nd| gen move {
        for op in then_jit_body { yield op; }
    })),
    else_ops: Box::new(else_jit.jit_core(|labels, nd| gen move {
        for op in else_jit_body { yield op; }
    })),
};
// no fall-through after If — both arms terminate the block
return;
```

**`Jal { dest, offset }`**
```rust
yield StackOp::PushImm(next_pc);
yield StackOp::StoreReg(dest.0);
// jit_core wraps the recursion in Br or Loop — no explicit branch needed here
```

**`Jalr { dest, base, offset }`**
```rust
yield StackOp::PushImm(next_pc);
yield StackOp::StoreReg(dest.0);
// dynamic target — exits the block; shim emits `return () => J(target)`
yield StackOp::Trap("jalr".into()); // placeholder until a DynamicJump variant is added
```

> **Note:** `Jalr` currently exits via `return ()=>J(addr)` in the JS output. A dedicated
> `StackOp::DynamicJump` variant should be added before implementing this step; marked here
> as a known gap.

### 7.3 Instruction integrity check

```rust
yield StackOp::CheckCode {
    addr: self.pc,
    expected: Box::from(inst_bytes),  // &[u8; 4] for RISC-V
    miss_label: alloc::format!("{}", self.params.root),
};
```

---

## 8. `Params` Simplification

`Flate` is removed from `Params`; it is a JS-only concern passed directly to `JsShim`.

```rust
// Before
pub struct Params<'a> {
    pub react: &'a dyn JitCtx,
    pub trial: &'a dyn Fn(u64) -> Heat,
    pub flate: &'a dyn Flate,          // ← removed
    pub root: u64,
    pub flags: Flags,
}

// After
pub struct Params<'a> {
    pub react: &'a dyn JitCtx,
    pub trial: &'a dyn Fn(u64) -> Heat,
    pub root: u64,
    pub flags: Flags,
}
```

`Flags` retains the paging fields — `JsShim` reads them when generating the `data` helper.

---

## 9. JS Shim — Virtual-Stack Compiler

The JS shim lives in `vane-target-js/src/shim.rs`. It owns `Flate` and `Flags`. It walks the
`StackOp` iterator maintaining a *virtual expression stack* of JS BigInt expression strings;
arithmetic and comparison ops pop inputs and push result expressions without emitting anything;
statements (stores, memory writes, control flow, side effects) drain the stack and emit JS.

```rust
pub struct JsShim<'a> {
    pub flate: &'a dyn Flate,
    pub flags: &'a Flags,
}

impl JsShim<'_> {
    pub fn compile(
        &self,
        ops: impl Iterator<Item = StackOp>,
        f:   &mut Formatter<'_>,
    ) -> core::fmt::Result {
        let mut stk: alloc::vec::Vec<alloc::string::String> = alloc::vec::Vec::new();
        let max64  = self.flate.flate("max64");
        let signed = self.flate.flate("signed");

        macro_rules! pop { () => { stk.pop().unwrap() } }
        macro_rules! bin { ($fmt:literal) => {{
            let b = pop!(); let a = pop!();
            stk.push(alloc::format!($fmt, a, b, max64 = max64, signed = signed));
        }} }

        for op in ops {
            match op {
                StackOp::PushImm(v) => stk.push(alloc::format!("{v}n")),

                StackOp::LoadReg(0) => stk.push("0n".into()),
                StackOp::LoadReg(r) => stk.push(
                    alloc::format!("(($._r??=$.r)[`x{r}`]??=0n)")
                ),
                StackOp::StoreReg(0) => { pop!(); }
                StackOp::StoreReg(r) => {
                    let v = pop!();
                    write!(f, "(($._r??=$.r)[`x{r}`]={v});")?;
                }

                StackOp::Add64  => bin!("(({0}+{1})&{max64})"),
                StackOp::Sub64  => bin!("(({0}-{1})&{max64})"),
                StackOp::And64  => bin!("(({0}&{1})&{max64})"),
                StackOp::Or64   => bin!("(({0}|{1})&{max64})"),
                StackOp::Xor64  => bin!("(({0}^{1})&{max64})"),
                StackOp::Shl64  => bin!("(({0}<<{1})&{max64})"),
                StackOp::ShrU64 => bin!("(({0}>>{1})&{max64})"),
                StackOp::ShrS64 => bin!("{signed}(({signed}({0})>>{1})&{max64})"),
                StackOp::Mul64  => bin!("(({0}*{1})&{max64})"),
                // … DivU64, DivS64, Rem*, MulH*, 32-bit variants follow same pattern …

                StackOp::Eq64  => bin!("(({0}==={1})?1n:0n)"),
                StackOp::Ne64  => bin!("(({0}!=={1})?1n:0n)"),
                StackOp::LtU64 => bin!("(({0}<{1})?1n:0n)"),
                StackOp::LtS64 => bin!("(({signed}({0})<{signed}({1}))?1n:0n)"),
                StackOp::GeU64 => bin!("(({0}>={1})?1n:0n)"),
                StackOp::GeS64 => bin!("(({signed}({0})>={signed}({1}))?1n:0n)"),

                StackOp::Trunc32   => {
                    let a = pop!();
                    let max32 = self.flate.flate("max32");
                    stk.push(alloc::format!("({a}&{max32})"));
                }
                StackOp::SignExt32 => {
                    let a = pop!();
                    let max32 = self.flate.flate("max32");
                    stk.push(alloc::format!("{signed}(BigInt.asIntN(32,{a}&{max32}))&{max64}"));
                }
                StackOp::AsUnsigned64 => {
                    let a = pop!();
                    let unsigned = self.flate.flate("unsigned");
                    stk.push(alloc::format!("{unsigned}({a})"));
                }
                StackOp::AsSigned64 => {
                    let a = pop!();
                    stk.push(alloc::format!("{signed}({a})"));
                }

                StackOp::LoadMem { width, signed: sgn } => {
                    let addr = pop!();
                    let data = self.flate.flate("data");
                    let getter = match (width, sgn) {
                        (MemWidth::B1, true)  => "getInt8(0,true)",
                        (MemWidth::B1, false) => "getUint8(0,true)",
                        (MemWidth::B2, true)  => "getInt16(0,true)",
                        (MemWidth::B2, false) => "getUint16(0,true)",
                        (MemWidth::B4, true)  => "getInt32(0,true)",
                        (MemWidth::B4, false) => "getUint32(0,true)",
                        (MemWidth::B8, _)     => "getBigUint64(0,true)",
                    };
                    let cast = if width == MemWidth::B8 { "" } else { "BigInt" };
                    stk.push(alloc::format!(
                        "{signed}({cast}({data}(({addr})&{max64}).{getter}))"
                    ));
                }
                StackOp::StoreMem { width } => {
                    let v    = pop!();
                    let addr = pop!();
                    let data = self.flate.flate("data");
                    let max32 = self.flate.flate("max32");
                    let (setter, val_expr) = match width {
                        MemWidth::B1 => ("setUint8(0,",    alloc::format!("Number({v}&{max32}),true")),
                        MemWidth::B2 => ("setUint16(0,",   alloc::format!("Number({v}&{max32}),true")),
                        MemWidth::B4 => ("setUint32(0,",   alloc::format!("Number({v}&{max32}),true")),
                        MemWidth::B8 => ("setBigUint64(0,",alloc::format!("{v},true")),
                    };
                    write!(f, "{data}(({addr})&{max64}).{setter}{val_expr});")?;
                }

                StackOp::Loop { label, body } => {
                    write!(f, "{label}: for(;;){{")?;
                    self.compile(body, f)?;
                    write!(f, "break {label};}}")?;
                }
                StackOp::Br { label, .. } => write!(f, "continue {label};")?,
                StackOp::If { then_ops, else_ops } => {
                    let cond = pop!();
                    write!(f, "if({cond}){{")?;
                    self.compile(then_ops, f)?;
                    write!(f, "}}else{{")?;
                    self.compile(else_ops, f)?;
                    write!(f, "}}")?;
                }

                StackOp::Ecall => write!(f, "await $.ecall();")?,
                StackOp::Trap(msg) => write!(f, "throw new TypeError(`{msg}`);")?,

                StackOp::CheckCode { addr, expected, miss_label } => {
                    let data = self.flate.flate("data");
                    // Emit a getUint32 check for 4-byte sequences; byte loop for others
                    match expected.len() {
                        4 => {
                            let word = u32::from_le_bytes(expected[..4].try_into().unwrap());
                            write!(f,
                                "if({data}({addr}n).getUint32(0,true)!=={word})\
                                 {{delete $.p[`{miss_label}`];return J({addr}n);}}"
                            )?;
                        }
                        n => {
                            for (i, b) in expected.iter().enumerate() {
                                write!(f,
                                    "if({data}({addr}n+{i}n).getUint8(0,true)!=={b})\
                                     {{delete $.p[`{miss_label}`];return J({addr}n);}}"
                                )?;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
```

### 9.1 `CoreJS` fate

`CoreJS` (the outer `async function` wrapper declaring `max64`, `signed`, `unsigned`, `data`,
etc.) stays as a `Display` wrapper. Its `content: &dyn Display` field is replaced by the
`StackOp` iterator, which `JsShim::compile` walks internally.

```rust
pub struct CoreJS<'a> {
    pub ops:   alloc::boxed::Box<dyn Iterator<Item = StackOp> + 'a>,
    pub flate: &'a dyn Flate,
    pub flags: Flags,
}
impl Display for CoreJS<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        // emit: return async function(){ let max64=$.f, max32=…, signed=…, unsigned=…, data=…;
        // … paging-aware data function body (unchanged from current CoreJS) …
        JsShim { flate: self.flate, flags: &self.flags }
            .compile(core::iter::from_fn(|| self.ops.next()), f)?;
        write!(f, "}}")
    }
}
```

---

## 10. WASM Shim — Thin Stack Lowering

WASM is already a stack machine, so the shim is thin. Registers map to **WASM function
locals** (index = register number), not globals — this avoids emitting a global section and
keeps the generated module compatible with the JS host interface.

```rust
// vane-target-wasm/src/shim.rs
pub fn lower_to_wasm(
    ops: impl Iterator<Item = StackOp>,
) -> impl Iterator<Item = Operator<'static>> {
    ops.flat_map(|op| -> alloc::vec::Vec<Operator<'static>> {
        use Operator::*;
        match op {
            StackOp::LoadReg(r)  => vec![LocalGet { local_index: r as u32 }],
            StackOp::StoreReg(r) => vec![LocalSet { local_index: r as u32 }],
            StackOp::PushImm(v)  => vec![I64Const { value: v as i64 }],

            StackOp::Add64  => vec![I64Add],
            StackOp::Sub64  => vec![I64Sub],
            StackOp::Mul64  => vec![I64Mul],
            StackOp::And64  => vec![I64And],
            StackOp::Or64   => vec![I64Or],
            StackOp::Xor64  => vec![I64Xor],
            StackOp::Shl64  => vec![I64Shl],
            StackOp::ShrU64 => vec![I64ShrU],
            StackOp::ShrS64 => vec![I64ShrS],
            StackOp::DivU64 => vec![I64DivU],
            StackOp::DivS64 => vec![I64DivS],
            StackOp::RemU64 => vec![I64RemU],
            StackOp::RemS64 => vec![I64RemS],

            StackOp::Eq64  => vec![I64Eq],
            StackOp::Ne64  => vec![I64Ne],
            StackOp::LtU64 => vec![I64LtU],
            StackOp::LtS64 => vec![I64LtS],
            StackOp::GeU64 => vec![I64GeU],
            StackOp::GeS64 => vec![I64GeS],

            StackOp::Trunc32   => vec![I64Const { value: 0xffff_ffff }, I64And],
            StackOp::SignExt32 => vec![I64Extend32S],
            StackOp::AsSigned64 | StackOp::AsUnsigned64 => vec![], // no-op in WASM

            StackOp::LoadMem { width, signed } => vec![match (width, signed) {
                (MemWidth::B1, true)  => I64Load8S  { memarg: default_memarg() },
                (MemWidth::B1, false) => I64Load8U  { memarg: default_memarg() },
                (MemWidth::B2, true)  => I64Load16S { memarg: default_memarg() },
                (MemWidth::B2, false) => I64Load16U { memarg: default_memarg() },
                (MemWidth::B4, true)  => I64Load32S { memarg: default_memarg() },
                (MemWidth::B4, false) => I64Load32U { memarg: default_memarg() },
                (MemWidth::B8, _)     => I64Load    { memarg: default_memarg() },
            }],
            StackOp::StoreMem { width } => vec![match width {
                MemWidth::B1 => I64Store8  { memarg: default_memarg() },
                MemWidth::B2 => I64Store16 { memarg: default_memarg() },
                MemWidth::B4 => I64Store32 { memarg: default_memarg() },
                MemWidth::B8 => I64Store   { memarg: default_memarg() },
            }],

            StackOp::Loop { label: _, body } => {
                core::iter::once(Loop { blockty: BlockType::Empty })
                    .chain(lower_to_wasm(body))
                    .chain(core::iter::once(End))
                    .collect()
            }
            StackOp::Br { relative_depth, .. } => vec![Br { relative_depth }],
            StackOp::If { then_ops, else_ops } => {
                core::iter::once(If { blockty: BlockType::Empty })
                    .chain(lower_to_wasm(then_ops))
                    .chain(core::iter::once(Else))
                    .chain(lower_to_wasm(else_ops))
                    .chain(core::iter::once(End))
                    .collect()
            }

            StackOp::Ecall | StackOp::CheckCode { .. } => vec![Unreachable],
            StackOp::Trap(_) => vec![Unreachable],

            // 32-bit ops: lower to i32 arithmetic then sign-extend
            StackOp::Add32 => vec![I32Add, I64ExtendI32S],
            // … etc …

            _ => vec![], // MulH* variants — not yet implemented in WASM
        }
    })
}
```

`JitOpcode`, `WasmJit`, `WasmJitCtx`, and `impl CoreOpcode for wasmparser::Operator` in
`vane-target-wasm` are all deleted. `lower_to_wasm` is the entire public API of the crate.

---

## 11. Dead Code Deletion

| Path | Action |
|---|---|
| `crates/vane-jit/` (entire crate) | Delete from filesystem and workspace |
| `TemplateJit::jit_js` | Delete |
| `TemplateJit::jit_wasm` | Delete |
| `ArchTarget::type Js` + `type Wasm` + `target_js`/`target_wasm` methods | Delete |
| `trait RiscvDisplay` (from `renders!`) | Delete |
| `trait RiscvWasmJit` / `RiscvWasmJitDyn` (from `renders!`) | Delete |
| `struct Riscv<'a, T>` wrapper newtype | Delete |
| `fn rv_core_js` | Delete |
| `TemplateReg` struct | Delete |
| `Params::flate` field | Delete |
| `vane-target-wasm::WasmJit` | Delete |
| `vane-target-wasm::WasmJitCtx` | Delete |
| `vane-target-wasm::JitOpcode` | Delete |
| `impl CoreOpcode for wasmparser::Operator` | Delete |
| `cfg_target_js!` / `cfg_target_wasm!` macro arms gating old traits | Delete |

---

## 12. Known Gaps (not blocked, track separately)

- **`Jalr` / dynamic jump**: needs a `StackOp::DynamicJump { target_expr }` variant that the
  JS shim lowers to `return ()=>J(target)`. Currently placeholder `Trap`.
- **`MulH*` in WASM**: no native 128-bit multiply in WASM i64; needs a helper function or
  explicit long-multiply sequence.
- **32-bit arithmetic edge cases** (`DivS32` signed overflow, `RemS32` with dividend = INT_MIN`):
  match current JS BigInt behaviour.

---

## 13. Migration Sequence

1. **Add `StackOp` to `vane-target-core`** — additive, no existing code changes.

2. **Fix `CoreOpcode::br` signature** — rename `index` → `relative_depth`; update all impls
   and `jit_core` call site.

3. **Add `JsShim`** in `vane-target-js`; wire into `CoreJS`. Keep old `CoreJS::content`
   path alive temporarily.

4. **Add `lower_to_wasm`** in `vane-target-wasm`.

5. **Rewrite `rv_core` in `vane-riscv`** — implement `RiscvArch for TemplateJit` emitting
   `StackOp` via gen block. Keep old `RiscvDisplay` alive until step 7.

6. **Remove `Flate` from `Params`**; update all call sites to pass it to `JsShim` / `CoreJS`.

7. **Update `renders!`** to generate only the `*Arch` trait; delete `*Display`, `*WasmJit`.

8. **Delete `vane-jit` crate** from workspace (`Cargo.toml` members, filesystem).

9. **Delete all remaining dead code** per §11.

10. **Smoke test**: `cargo build --all-features`, `rv_corpus` integration tests, `wasm-pack test`.
