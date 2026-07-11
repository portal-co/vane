//! Server-side execution of vane's `StackOp`/JIT-compiled RISC-V blocks
//! against a real browser tab, connected over WebSocket via `wsdom`
//! (see the plan's "Mode B — server hosts, browser executes").
//!
//! The server compiles one basic-block trace at a time — the exact same
//! `StackOp -> JS` pipeline the in-browser wasm-bindgen build uses
//! (`render_stack_to_js`/`CoreJS`, unmodified) — and ships it down with
//! [`wsdom::Browser::run_raw_code`]/`value_from_raw_code`, then awaits the
//! block's exit (the next PC to run, encoded as a decimal string) before
//! compiling and shipping the next block. Registers/memory live entirely on
//! the browser side between blocks; the server never reads them back except
//! through that exit value.
//!
//! This crate does not persist a cross-call JIT cache: every call to
//! [`run_block`] traces fresh from its root PC (`trial` always reports
//! [`Heat::New`]), so repeated visits to the same PC across separate `run`
//! calls recompile it. That's a throughput cost, not a correctness one —
//! within a single trace, backward branches still fold into `StackOp::Br`
//! loops via the existing `Labels` bookkeeping in `rv_emit`.

use core::fmt;

use vane_riscv::{
    DebugFlate, Heat, JitCtx, Mem, Riscv,
    template::{CoreJS, Flags, Labels, Params, TemplateJit},
};
use wsdom::{Browser, JsCast, js_types::JsString};
use wsdom_core::UseInJsCode;

/// Bytes in the flat guest-memory `ArrayBuffer` allocated on the browser side
/// by [`bootstrap`]. Addresses wrap (`addr % DEFAULT_MEM_SIZE`) rather than
/// going through `vane_riscv::Mem`'s page-table modes — a deliberate v1
/// simplification (see the plan's Phase 2), not a reimplementation of
/// paging. Pass a different size to [`bootstrap`] if this doesn't fit.
pub const DEFAULT_MEM_SIZE: u64 = 16 * 1024 * 1024;

/// Wraps a `&dyn UseInJsCode` so it can be interpolated into the
/// `format_args!` calls passed to `Browser::run_raw_code`/`value_from_raw_code`.
/// Equivalent to wsdom-core's own (crate-private) `UseInJsCodeWriter`, which
/// isn't reachable from outside that crate.
struct AsJs<'a>(&'a dyn UseInJsCode);
impl fmt::Display for AsJs<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.serialize_to(f)
    }
}

#[derive(Debug)]
pub enum Error {
    /// The block's exit value couldn't be retrieved from the browser (e.g.
    /// the connection dropped, or — per wsdom's documented limitation on JS
    /// exceptions — the block hit a `StackOp::Trap` and the settled value
    /// wasn't the string `J` produces).
    Retrieve,
    /// The exit value was retrieved but wasn't a valid decimal `u64`.
    Parse(String),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Retrieve => write!(f, "failed to retrieve block exit value from browser"),
            Error::Parse(s) => write!(f, "block exit value {s:?} is not a valid u64"),
        }
    }
}
impl core::error::Error for Error {}

/// Send the browser-side `$` runtime object and `J` helper. Must be called
/// once per connection before [`run_block`]/[`run`].
///
/// `$` here is a minimal, self-contained stand-in for the object the
/// wasm-bindgen build's inline JS glue provides
/// (`crates/vane-meta-gen/src/lib.rs`): a flat register file (`$.r`), a flat
/// guest-memory buffer reachable via `$._sys('memory')`/`$.get_page`, and
/// `$.ecall` handling the `exit` syscall (`a7==93`) locally — matching
/// `Reactor::_ecall` (`crates/vane/src/lib.rs`). Unlike the wasm-bindgen
/// build, register/memory state is *not* mirrored back to the server between
/// blocks: it stays in the browser's `$` object for the connection's
/// lifetime, matching Mode B in the plan.
pub async fn bootstrap(browser: &Browser, mem_size: u64) {
    let code = format!(
        "globalThis.$={{\
            mem:new ArrayBuffer({mem_size}),\
            r:{{}},\
            get_page(a){{return Number(a%{mem_size}n);}},\
            _sys(name){{return name==='memory'?{{buffer:this.mem}}:undefined;}},\
            async ecall(){{if((this.r['x17']??0n)===93n){{throw new Error('vane:exit');}}}}\
        }};\
        globalThis.J=function(addr){{return String(addr);}};"
    );
    browser.run_raw_code(format_args!("{code}"));
}

/// One JIT-compiled trace's outcome: the PC execution should resume at.
#[derive(Debug, Clone, Copy)]
pub struct BlockExit {
    pub next_pc: u64,
}

/// Render the trace rooted at `pc` to its `CoreJS` JavaScript text.
///
/// This reuses `CoreJS`/`render_stack_to_js` exactly as `vane-meta-gen`'s
/// `jit_code` does for the in-browser build — no `StackOp`/codegen changes
/// are needed for Mode B, since `CoreJS`'s output is already an
/// `async function` and `StackOp::Ecall` already lowers to `await
/// $.ecall();` inside it.
///
/// Deliberately a plain (non-`async`) function: `Params`/`TemplateJit` hold
/// `&dyn Trait` references that aren't `Sync`, and keeping their computation
/// entirely off the `async fn` stack (rather than inline in [`run_block`])
/// keeps the block-dispatch future `Send`, which axum requires to spawn
/// connection futures on its multi-threaded runtime.
fn compile_block(mem: &Mem, pc: u64, flags: Flags) -> String {
    let flate = DebugFlate::default();
    let params = Params {
        react: mem as &dyn JitCtx,
        trial: &|_| Heat::New,
        flate: &flate,
        root: pc,
        flags,
    };
    let jit = TemplateJit {
        params,
        pc,
        labels: Labels::default(),
        depth: 0,
    };
    CoreJS {
        content: &Riscv(&jit),
        flate: &flate,
        flags,
    }
    .to_string()
}

/// Compile the trace rooted at `pc` and ship it to `browser` for execution,
/// awaiting its exit.
pub async fn run_block(browser: &Browser, mem: &Mem, pc: u64, flags: Flags) -> Result<BlockExit, Error> {
    let body = compile_block(mem, pc, flags);

    // `body` is `return async function(){ ... };` — wrap it in an IIFE so we
    // get an *expression* back (the inner async function value) from
    // `value_from_raw_code`, which wants an expression, not a statement.
    let async_fn = browser.value_from_raw_code(format_args!("(function(){{{body}}})()"));
    // Call it with no arguments to get the Promise it returns.
    let promise = browser.value_from_raw_code(format_args!("({})()", AsJs(&async_fn)));
    // Await the Promise settling. Per wsdom's documented limitation, this
    // resolves even when the underlying JS threw (e.g. StackOp::Trap) — see
    // wsdom's README "Roundtrip-Free Calls" section — so a trapped block
    // surfaces as a Retrieve/Parse error below rather than a typed error here.
    let settled = promise.await;
    // Every trace terminates in `return ()=>J(addr);` (StackOp::TailCall), so
    // `settled` is that closure. `J` is synchronous, so calling it resolves
    // immediately to a JS string — no further await needed.
    let addr_str = browser.value_from_raw_code(format_args!("({})()", AsJs(&settled)));
    let addr_str: JsString = JsCast::unchecked_from_js(addr_str);
    let s = addr_str.retrieve().await.map_err(|_| Error::Retrieve)?;
    let next_pc = s.parse::<u64>().map_err(|_| Error::Parse(s))?;
    Ok(BlockExit { next_pc })
}

/// Drive block-by-block execution starting at `entry_pc`, stopping when
/// `should_stop` returns `true` for the *next* PC (checked before that block
/// is compiled/dispatched), or when a block fails to resolve.
pub async fn run(
    browser: &Browser,
    mem: &Mem,
    entry_pc: u64,
    flags: Flags,
    mut should_stop: impl FnMut(u64) -> bool,
) -> Result<u64, Error> {
    let mut pc = entry_pc;
    while !should_stop(pc) {
        let exit = run_block(browser, mem, pc, flags).await?;
        pc = exit.next_pc;
    }
    Ok(pc)
}

/// Drive a single WebSocket connection end to end: bootstrap the browser-side
/// `$` runtime, then run blocks starting at `entry_pc` until `should_stop`
/// returns `true`.
pub async fn serve_connection(
    socket: axum::extract::ws::WebSocket,
    mem: Mem,
    entry_pc: u64,
    flags: Flags,
    mem_size: u64,
    mut should_stop: impl FnMut(u64) -> bool + Send + 'static,
) {
    wsdom_axum::socket_to_browser(socket, move |browser| async move {
        bootstrap(&browser, mem_size).await;
        let _ = run(&browser, &mem, entry_pc, flags, &mut should_stop).await;
    })
    .await;
}
