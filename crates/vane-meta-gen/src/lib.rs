pub use vane_jit;
pub use wasm_bindgen;
#[macro_export]
macro_rules! vane_meta {
    ($t:ident, $c:ident, $y:expr) => {
        #[$crate::wasm_bindgen::prelude::wasm_bindgen(wasm_bindgen = $crate::wasm_bindgen)]
        #[derive(Clone)]
        pub struct $t {
            _handle: (),
            core: Rc<Mutex<$c>>,
        }
        impl JitCtx for Reactor {
            fn bytes(&self, a: u64) -> Box<dyn Iterator<Item = u8> + '_> {
                Box::new((a..).filter_map(move |a| {
                    let mut lock = self.core.lock().unwrap();
                    let n = lock.mem.bytes(a).next()?;
                    Some(n)
                }))
            }
        }
        struct $c {
            mem: Mem,
            state: OnceCell<JsValue>,
            regs: OnceCell<JsValue>,
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
                fn get(a: $t, b: u64) -> JsValue;
                fn tget(a: $t, b: u64) -> JsValue;
                fn on() -> JsValue;
                fn l(a: JsValue) -> Promise;
                fn reg(a: $t, b: u8) -> u64;
                fn set_reg(a: $t, b: u8, c: u64) -> u64;
                fn get_memory(a: JsValue) -> JsValue;

                #[wasm_bindgen(catch)]
                async fn jit_run(a: JsValue) -> Result<JsValue, JsValue>;
                //   #[wasm_bindgen(thread_local_v2, js_name = "memory")]
                // static MEM_HANDLE: JsValue;
            }
            #[$crate::wasm_bindgen::prelude::wasm_bindgen(wasm_bindgen = $crate::wasm_bindgen)]
            extern "C" {
                fn eval(a: &str) -> JsValue;
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
                pub fn state(&self) -> JsValue {
                    let mut lock = self.core.lock().unwrap();
                    return lock.state.get_or_init(|| on()).clone();
                }
                #[wasm_bindgen(getter, js_name = "r",wasm_bindgen = $crate::wasm_bindgen)]
                pub fn regs(&self) -> JsValue {
                    let mut lock = self.core.lock().unwrap();
                    return lock.regs.get_or_init(|| on()).clone();
                }
                #[wasm_bindgen(wasm_bindgen = $crate::wasm_bindgen)]
                pub fn _sys(&self, a: &str) -> JsValue {
                    match a {
                        "memory" => get_memory(eval("wasm")),
                        _ => JsValue::undefined(),
                    }
                }
                #[wasm_bindgen(wasm_bindgen = $crate::wasm_bindgen)]
                pub fn get_page(&self, a: u64) -> *mut u8 {
                    let mut lock = self.core.lock().unwrap();
                    match &mut lock.mem {
                        m => m.get_page(a),
                    }
                }
                #[wasm_bindgen(js_name = "J",wasm_bindgen = $crate::wasm_bindgen)]
                pub fn jit(&self, a: u64) -> JsValue {
                    return get(self.clone(), a);
                }
                #[wasm_bindgen]
                pub async fn jit_run(&self, mut pc: u64) -> Result<JsValue, JsValue> {
                    let j = self.jit(pc);
                    return jit_run(j).await;
                }
                #[wasm_bindgen(js_name = "j",wasm_bindgen = $crate::wasm_bindgen)]
                pub fn jit_code(&self, a: u64) -> String {
                    return CoreJS(&$y(&TemplateJit {
                        params: Params {
                            react: self,
                            trial: &|a| match tget(self.clone(), a) != JsValue::UNDEFINED {
                                true => Heat::Cached,
                                false => Heat::New,
                            },
                            root: a,
                        },
                        pc: a,
                        labels: &BTreeMap::default(),
                        depth: 0,
                    }))
                    .to_string();
                }
                #[wasm_bindgen(getter, js_name = "f",wasm_bindgen = $crate::wasm_bindgen)]
                pub fn u64_max(&self) -> u64 {
                    u64::MAX
                }
            }
        };
    };
}
