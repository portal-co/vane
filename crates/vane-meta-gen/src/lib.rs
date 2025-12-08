#![no_std]
#[doc(hidden)]
pub use core;
#[doc(hidden)]
pub extern crate alloc;
pub use spin;
pub use vane_jit;
pub use wasm_bindgen;
#[macro_export]
macro_rules! vane_meta {
    ($t:ident, $c:ident, $y:expr, $flate:expr) => {
        #[$crate::wasm_bindgen::prelude::wasm_bindgen(wasm_bindgen = $crate::wasm_bindgen)]
        #[derive(Clone)]
        pub struct $t {
            _handle: (),
            core: $crate::alloc::rc::Rc<$crate::spin::Mutex<$c>>,
        }
        impl $crate::vane_jit::JitCtx for Reactor {
            fn bytes(&self, a: u64) -> $crate::alloc::boxed::Box<dyn Iterator<Item = u8> + '_> {
                $crate::alloc::boxed::Box::new((a..).filter_map(move |a| {
                    let mut lock = self.core.lock();
                    let n = lock.mem.bytes(a).next()?;
                    Some(n)
                }))
            }
        }
        struct $c {
            mem: $crate::vane_jit::Mem,
            state: $crate::core::cell::OnceCell<$crate::wasm_bindgen::prelude::JsValue>,
            regs: $crate::core::cell::OnceCell<$crate::wasm_bindgen::prelude::JsValue>,
            test_mode: bool,
        }

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
                
                #[wasm_bindgen(js_name = "get_paging_mode",wasm_bindgen = $crate::wasm_bindgen)]
                pub fn get_paging_mode(&self) -> String {
                    match self.core.lock().mem.paging_mode {
                        $crate::vane_jit::PagingMode::Legacy => "legacy".to_string(),
                        $crate::vane_jit::PagingMode::Shared => "shared".to_string(),
                        $crate::vane_jit::PagingMode::Both => "both".to_string(),
                    }
                }
                
                #[wasm_bindgen(js_name = "set_paging_mode",wasm_bindgen = $crate::wasm_bindgen)]
                pub fn set_paging_mode(&self, mode: &str) {
                    let paging_mode = match mode {
                        "legacy" => $crate::vane_jit::PagingMode::Legacy,
                        "shared" => $crate::vane_jit::PagingMode::Shared,
                        "both" => $crate::vane_jit::PagingMode::Both,
                        _ => $crate::vane_jit::PagingMode::Legacy,
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
                    drop(lock);
                    
                    let flags = $crate::vane_jit::template::Flags::with_paging(
                        test_mode,
                        paging_mode,
                        shared_page_table_vaddr,
                        shared_security_directory_vaddr,
                        false, // use_32bit_paging
                        false, // use_multilevel_paging
                    );
                    
                    return ($crate::vane_jit::template::CoreJS {
                        content: &$y(&$crate::vane_jit::template::TemplateJit {
                            params: Params {
                                react: self,
                                trial: &|a| match tget(self.clone(), a)
                                    != $crate::wasm_bindgen::prelude::JsValue::UNDEFINED
                                {
                                    true => $crate::vane_jit::Heat::Cached,
                                    false => $crate::vane_jit::Heat::New,
                                },
                                root: a,
                                flate: &f,
                                flags,
                            },
                            pc: a,
                            labels: &$crate::vane_jit::template::Labels::default(),
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
