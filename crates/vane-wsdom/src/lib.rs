//! Browser execution of Vane traces with an explicit server-owned Unix host.
//!
//! Guest registers and memory remain in the WSDOM browser between traces.  A
//! syscall is different: `$.ecall()` posts an address-free request to the
//! server, waits for its matching response, and only then resumes the trace.
//! The server snapshots only the requested guest buffer, invokes its
//! synchronous [`os_host_api::HostApi`], copies read data back, and resolves
//! the browser Promise.  There is deliberately no browser-local fallback.

use core::{fmt, future::IntoFuture};
use std::string::String;

use futures_util::StreamExt;
use os_host_api::{HostApi, UnixHostAdapter};
use os_unix_emulation::{Abi, EFAULT, GuestMemory, Outcome, Syscall, dispatch_args};
use vane_aarch64::Aarch64;
use vane_riscv::{
    DebugFlate, Heat, JitCtx, Mem, Riscv,
    template::{CoreJS, Flags, Labels, Params, TemplateJit},
};
use wsdom::{
    Browser, JsCast, callback,
    js_types::{JsString, JsValue},
};
use wsdom_core::UseInJsCode;

/// Bytes in the flat browser guest-memory `ArrayBuffer` allocated by
/// [`bootstrap`].  Syscall buffers larger than this fail with `-EFAULT`.
pub const DEFAULT_MEM_SIZE: u64 = 16 * 1024 * 1024;

/// The Vane guest frontend selected for a WSDOM connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuestAbi {
    LinuxRv64,
    LinuxAarch64,
    DarwinAarch64,
}

impl GuestAbi {
    pub const fn unix_abi(self) -> Abi {
        match self {
            Self::LinuxRv64 => Abi::LinuxRv64,
            Self::LinuxAarch64 => Abi::LinuxAarch64,
            Self::DarwinAarch64 => Abi::DarwinAarch64,
        }
    }

    fn state_key(self, reg: u8) -> String {
        match self {
            Self::LinuxRv64 => format!("x{reg}"),
            Self::LinuxAarch64 | Self::DarwinAarch64 => format!("a64:x{reg}"),
        }
    }
}

/// Wraps `UseInJsCode` so it can be interpolated into a `format_args!` call.
struct AsJs<'a>(&'a dyn UseInJsCode);
impl fmt::Display for AsJs<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.serialize_to(f)
    }
}

#[derive(Debug)]
pub enum Error {
    Retrieve,
    Parse(String),
    Protocol(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Retrieve => write!(f, "failed to retrieve a browser value"),
            Self::Parse(value) => write!(f, "invalid browser block exit: {value:?}"),
            Self::Protocol(value) => write!(f, "invalid browser syscall request: {value}"),
        }
    }
}
impl core::error::Error for Error {}

/// One JIT trace outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockExit {
    NextPc(u64),
    Exit(u32),
}

/// Whole connection outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunExit {
    NextPc(u64),
    Exit(u32),
}

/// Install the browser runtime with a mandatory server syscall callback.
///
/// `host_callback` must be the function returned by
/// [`wsdom::callback::new_callback`].  It is explicitly injected rather than
/// discovered by name, so WSDOM/browser execution cannot fall back to local
/// browser syscall handling.
pub fn bootstrap(browser: &Browser, mem_size: u64, abi: GuestAbi, host_callback: &dyn UseInJsCode) {
    assert!(mem_size != 0, "browser guest memory must not be empty");
    let registers = abi.unix_abi().registers();
    let number = abi.state_key(registers.number);
    let result = abi.state_key(registers.result);
    let args = registers.args.map(|reg| abi.state_key(reg));
    let number = serde_json::to_string(&number).expect("state key is serializable");
    let result = serde_json::to_string(&result).expect("state key is serializable");
    let args = args.map(|key| serde_json::to_string(&key).expect("state key is serializable"));
    let code = format!(
        "globalThis.$={{\
            f:{max_u64}n,\
            mem:new ArrayBuffer({mem_size}),\
            r:{{}},\
            p:{{}},\
            _pending_syscalls:new Map(),\
            _next_syscall:0,\
            _host_syscall:{callback},\
            get_page(a){{return Number(a%{mem_size}n);}},\
            _sys(name){{return name==='memory'?{{buffer:this.mem}}:undefined;}},\
            async ecall(){{\
                const id=this._next_syscall++;\
                const request={{id,nr:String(this.r[{number}]??0n),args:[\
                    String(this.r[{arg0}]??0n),String(this.r[{arg1}]??0n),String(this.r[{arg2}]??0n)\
                ]}};\
                return await new Promise(resolve=>{{\
                    this._pending_syscalls.set(id,resolve);\
                    this._host_syscall(JSON.stringify(request));\
                }});\
            }},\
            _resume_syscall(id,result,exit){{\
                const resolve=this._pending_syscalls.get(id);\
                if(!resolve)return;\
                this._pending_syscalls.delete(id);\
                if(exit!==null){{resolve({{kind:'exit',code:exit}});return;}}\
                this.r[{result}]=BigInt.asUintN(64,BigInt(result));\
                resolve(undefined);\
            }}\
        }};\
        globalThis.J=function(addr){{return String(addr);}};",
        max_u64 = u64::MAX,
        callback = AsJs(host_callback),
        arg0 = args[0],
        arg1 = args[1],
        arg2 = args[2],
    );
    browser.run_raw_code(format_args!("{code}"));
}

/// Copy every populated 64KB page of `mem` into browser guest memory.
pub async fn seed_memory(browser: &Browser, mem: &Mem, mem_size: u64) {
    for (&page_no, page) in mem.pages.iter() {
        let page_base = page_no << 16;
        let offset = page_base % mem_size;
        let bytes = page.iter().map(u8::to_string).collect::<Vec<_>>().join(",");
        browser.run_raw_code(format_args!(
            "(new Uint8Array($.mem)).set([{bytes}],{offset});"
        ));
    }
}

/// Render one trace with the frontend selected for this connection.
fn compile_block(mem: &Mem, pc: u64, flags: Flags, abi: GuestAbi) -> String {
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
    match abi {
        GuestAbi::LinuxRv64 => CoreJS {
            content: &Riscv(&jit),
            flate: &flate,
            flags,
        }
        .to_string(),
        GuestAbi::LinuxAarch64 | GuestAbi::DarwinAarch64 => CoreJS {
            content: &Aarch64(&jit),
            flate: &flate,
            flags,
        }
        .to_string(),
    }
}

#[derive(Debug)]
struct SyscallRequest {
    id: u64,
    number: u64,
    args: [u64; 3],
}

fn parse_request(value: &str) -> Result<SyscallRequest, Error> {
    let value: serde_json::Value =
        serde_json::from_str(value).map_err(|error| Error::Protocol(error.to_string()))?;
    let get_u64 = |key: &str| -> Result<u64, Error> {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| Error::Protocol(format!("missing {key}")))?
            .parse()
            .map_err(|_| Error::Protocol(format!("invalid {key}")))
    };
    let id = value
        .get("id")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| Error::Protocol("missing id".into()))?;
    let args = value
        .get("args")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| Error::Protocol("missing args".into()))?;
    if args.len() != 3 {
        return Err(Error::Protocol(
            "args must have exactly three entries".into(),
        ));
    }
    let mut parsed_args = [0; 3];
    for (slot, value) in parsed_args.iter_mut().zip(args) {
        *slot = value
            .as_str()
            .ok_or_else(|| Error::Protocol("argument was not a string".into()))?
            .parse()
            .map_err(|_| Error::Protocol("invalid argument".into()))?;
    }
    Ok(SyscallRequest {
        id,
        number: get_u64("nr")?,
        args: parsed_args,
    })
}

struct RemoteBuffer {
    address: u64,
    bytes: Vec<u8>,
    valid: bool,
}

impl RemoteBuffer {
    fn new(address: u64, length: u64, mem_size: u64) -> Self {
        let valid = usize::try_from(length).is_ok() && length <= mem_size;
        Self {
            address,
            bytes: valid.then(|| vec![0; length as usize]).unwrap_or_default(),
            valid,
        }
    }
}

impl GuestMemory for RemoteBuffer {
    fn read(&mut self, address: u64, bytes: &mut [u8]) -> Result<(), i64> {
        if !self.valid || address != self.address || bytes.len() != self.bytes.len() {
            return Err(-EFAULT);
        }
        bytes.copy_from_slice(&self.bytes);
        Ok(())
    }

    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<(), i64> {
        if !self.valid || address != self.address || bytes.len() != self.bytes.len() {
            return Err(-EFAULT);
        }
        self.bytes.copy_from_slice(bytes);
        Ok(())
    }
}

async fn get_guest_buffer(
    browser: &Browser,
    address: u64,
    length: u64,
    mem_size: u64,
) -> Result<Vec<u8>, Error> {
    if usize::try_from(length).is_err() || length > mem_size {
        return Err(Error::Protocol(
            "guest buffer exceeds browser memory".into(),
        ));
    }
    let value = browser.value_from_raw_code(format_args!(
        "JSON.stringify((()=>{{const memory=new Uint8Array($.mem),start=$.get_page({address}n);return Array.from({{length:{length}}},(_,index)=>memory[(start+index)%{mem_size}]);}})())"
    ));
    let value: JsString = JsCast::unchecked_from_js(value);
    let value = value.retrieve().await.map_err(|_| Error::Retrieve)?;
    serde_json::from_str(&value).map_err(|error| Error::Protocol(error.to_string()))
}

async fn put_guest_buffer(browser: &Browser, address: u64, bytes: &[u8], mem_size: u64) {
    let length = bytes.len();
    let bytes = serde_json::to_string(bytes).expect("byte buffers are serializable");
    browser.run_raw_code(format_args!(
        "(()=>{{const memory=new Uint8Array($.mem),start=$.get_page({address}n),bytes={bytes};for(let index=0;index<{length};index++)memory[(start+index)%{mem_size}]=bytes[index];}})();",
    ));
}

async fn handle_syscall<H: HostApi + ?Sized>(
    browser: &Browser,
    host_api: &mut H,
    abi: GuestAbi,
    mem_size: u64,
    request: SyscallRequest,
) -> Result<(), Error> {
    let decoded = abi.unix_abi().decode(request.number);
    let mut remote = RemoteBuffer::new(request.args[1], request.args[2], mem_size);
    if matches!(decoded, Some(Syscall::Write)) && remote.valid {
        remote.bytes = get_guest_buffer(browser, remote.address, request.args[2], mem_size).await?;
    }
    let mut host = UnixHostAdapter::new(host_api);
    let outcome = dispatch_args(
        abi.unix_abi(),
        request.number,
        request.args,
        &mut remote,
        &mut host,
    );
    if matches!(decoded, Some(Syscall::Read)) {
        if let Outcome::Return(count) = outcome {
            let count = usize::try_from(count.max(0))
                .unwrap_or(usize::MAX)
                .min(remote.bytes.len());
            if count != 0 {
                put_guest_buffer(browser, remote.address, &remote.bytes[..count], mem_size).await;
            }
        }
    }
    match outcome {
        Outcome::Return(result) => browser.run_raw_code(format_args!(
            "$._resume_syscall({id},{result},null);",
            id = request.id,
        )),
        Outcome::Exit(code) => browser.run_raw_code(format_args!(
            "$._resume_syscall({id},0,{code});",
            id = request.id,
        )),
    }
    Ok(())
}

async fn settled_block_exit(settled: JsValue, browser: &Browser) -> Result<BlockExit, Error> {
    let value = browser.value_from_raw_code(format_args!(
        "JSON.stringify(typeof {settled}==='function'?{{kind:'next',pc:String(({settled})())}}:{settled})",
        settled = AsJs(&settled),
    ));
    let value: JsString = JsCast::unchecked_from_js(value);
    let value = value.retrieve().await.map_err(|_| Error::Retrieve)?;
    let value: serde_json::Value =
        serde_json::from_str(&value).map_err(|_| Error::Parse(value.clone()))?;
    match value.get("kind").and_then(serde_json::Value::as_str) {
        Some("next") => value
            .get("pc")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| Error::Parse(value.to_string()))?
            .parse()
            .map(BlockExit::NextPc)
            .map_err(|_| Error::Parse(value.to_string())),
        Some("exit") => value
            .get("code")
            .and_then(serde_json::Value::as_u64)
            .and_then(|code| u32::try_from(code).ok())
            .map(BlockExit::Exit)
            .ok_or_else(|| Error::Parse(value.to_string())),
        _ => Err(Error::Parse(value.to_string())),
    }
}

/// Compile and execute one block, servicing its server-owned syscall bridge.
pub async fn run_block<H: HostApi + ?Sized>(
    browser: &Browser,
    callbacks: &mut callback::Callback<JsValue>,
    host_api: &mut H,
    abi: GuestAbi,
    mem: &Mem,
    mem_size: u64,
    pc: u64,
    flags: Flags,
) -> Result<BlockExit, Error> {
    let body = compile_block(mem, pc, flags, abi);
    let async_fn = browser.value_from_raw_code(format_args!("(function(){{{body}}})()"));
    let promise = browser
        .value_from_raw_code(format_args!("({})()", AsJs(&async_fn)))
        .into_future();
    tokio::pin!(promise);
    loop {
        tokio::select! {
            settled = &mut promise => return settled_block_exit(settled, browser).await,
            request = callbacks.next() => {
                let Some(request) = request else { return Err(Error::Retrieve); };
                let request: JsString = JsCast::unchecked_from_js(request);
                let request = request.retrieve().await.map_err(|_| Error::Retrieve)?;
                handle_syscall(browser, host_api, abi, mem_size, parse_request(&request)?).await?;
            }
        }
    }
}

/// Drive blocks until the caller's normal control-flow stop condition or a
/// terminal Unix exit.  The host API is mandatory and remains solely server
/// owned for the full run.
pub async fn run<H: HostApi + ?Sized>(
    browser: &Browser,
    callbacks: &mut callback::Callback<JsValue>,
    host_api: &mut H,
    abi: GuestAbi,
    mem: &Mem,
    mem_size: u64,
    entry_pc: u64,
    flags: Flags,
    mut should_stop: impl FnMut(u64) -> bool,
) -> Result<RunExit, Error> {
    let mut pc = entry_pc;
    while !should_stop(pc) {
        match run_block(browser, callbacks, host_api, abi, mem, mem_size, pc, flags).await? {
            BlockExit::NextPc(next_pc) => pc = next_pc,
            BlockExit::Exit(code) => return Ok(RunExit::Exit(code)),
        }
    }
    Ok(RunExit::NextPc(pc))
}

/// Serve one WSDOM connection with an explicit, policy-carrying host API.
pub async fn serve_connection<H: HostApi + 'static>(
    socket: axum::extract::ws::WebSocket,
    mem: Mem,
    entry_pc: u64,
    flags: Flags,
    mem_size: u64,
    abi: GuestAbi,
    mut host_api: H,
    mut should_stop: impl FnMut(u64) -> bool + Send + 'static,
) -> wsdom_axum::Output<Result<RunExit, Error>> {
    wsdom_axum::socket_to_browser(socket, move |browser| async move {
        let (mut callbacks, callback) = callback::new_callback::<JsValue>(&browser);
        bootstrap(&browser, mem_size, abi, &callback);
        seed_memory(&browser, &mem, mem_size).await;
        run(
            &browser,
            &mut callbacks,
            &mut host_api,
            abi,
            &mem,
            mem_size,
            entry_pc,
            flags,
            &mut should_stop,
        )
        .await
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_protocol_preserves_full_u64_arguments() {
        let request =
            parse_request(r#"{"id":7,"nr":"18446744073709551615","args":["1","2","3"]}"#).unwrap();
        assert_eq!(request.id, 7);
        assert_eq!(request.number, u64::MAX);
        assert_eq!(request.args, [1, 2, 3]);
    }

    #[test]
    fn remote_buffer_rejects_invalid_lengths() {
        let mut buffer = RemoteBuffer::new(0x100, DEFAULT_MEM_SIZE + 1, DEFAULT_MEM_SIZE);
        let mut byte = [0];
        assert_eq!(buffer.read(0x100, &mut byte), Err(-EFAULT));
        assert_eq!(buffer.write(0x100, &byte), Err(-EFAULT));
    }

    #[test]
    fn every_browser_abi_uses_its_shared_unix_descriptor() {
        assert_eq!(GuestAbi::LinuxRv64.unix_abi(), Abi::LinuxRv64);
        assert_eq!(GuestAbi::LinuxAarch64.unix_abi(), Abi::LinuxAarch64);
        assert_eq!(GuestAbi::DarwinAarch64.unix_abi(), Abi::DarwinAarch64);
        assert_eq!(GuestAbi::LinuxRv64.state_key(17), "x17");
        assert_eq!(GuestAbi::LinuxAarch64.state_key(8), "a64:x8");
        assert_eq!(GuestAbi::DarwinAarch64.state_key(16), "a64:x16");
    }
}
