mod utils;

use std::{
    cell::{OnceCell, UnsafeCell},
    collections::BTreeMap,
    fmt::Display,
    ptr::null_mut,
    rc::Rc,
    u64,
};

use js_sys::Promise;
use rv_asm::{Inst, Reg, Xlen};
use wasm_bindgen::prelude::*;

#[wasm_bindgen(raw_module = "./vane_bg.wasm")]
extern "C" {
    #[wasm_bindgen(thread_local_v2, js_name = "memory")]
    static MEM_HANDLE: JsValue;
}
// #[wasm_bindgen(raw_module = "./vane.js")]
// extern "C"{
//     #[wasm_bindgen(js_name = "Reactor")]
//     type Reactor2;
// }

#[wasm_bindgen(inline_js = r#"
    export function get(a,b){
        return a.p[`${b}`]??=(new Function("$","J",a.j(b))(a,b=>get(a,b)))
    }
    export function on(){
        return Object.create(null)
    }
    export function tget(a,b){
        return a.p[`${b}`]
    }
    export async function l(a){
        while(typeof a === "function")a = await a();
        return a;
    }"#)]
extern "C" {
    fn get(a: Reactor, b: u64) -> JsValue;
    fn tget(a: Reactor, b: u64) -> JsValue;
    fn on() -> JsValue;
    fn l(a: JsValue) -> Promise;
}

#[wasm_bindgen]
#[derive(Clone)]
pub struct Reactor {
    _handle: (),
    core: Rc<UnsafeCell<Core>>,
}

struct Core {
    pages: BTreeMap<u64, [u8; 65536]>,
    dollar: OnceCell<JsValue>,
    dollar2: OnceCell<JsValue>,
    regs: [u64; 31], // regs: Regs,
}

#[wasm_bindgen]
impl Reactor {
    #[wasm_bindgen(getter, js_name = "p")]
    pub fn dollar(&self) -> JsValue {
        return unsafe { &mut *self.core.get() }
            .dollar
            .get_or_init(|| on())
            .clone();
    }
    #[wasm_bindgen(getter, js_name = "r")]
    pub fn dollar2(&self) -> JsValue {
        return unsafe { &mut *self.core.get() }
            .dollar2
            .get_or_init(|| on())
            .clone();
    }
    #[wasm_bindgen]
    pub fn _sys(&self, a: &str) -> JsValue {
        match a {
            "memory" => MEM_HANDLE.with(|a| a.clone()),
            _ => JsValue::undefined(),
        }
    }
    #[wasm_bindgen]
    pub fn get_page(&self, a: u64) -> *mut u8 {
        match unsafe { &mut *self.core.get() }
            .pages
            .entry((a >> 16))
            .or_insert_with(|| [0u8; 65536])
        {
            p => &raw mut p[(a & 0xffff) as usize],
        }
    }
    #[wasm_bindgen]
    pub fn J(&self, a: u64) -> JsValue {
        return get(self.clone(), a);
    }
    #[wasm_bindgen]
    pub fn j(&self, a: u64) -> String {
        return format!(
            "async ()=>{{let f=$.f,g=0xffff_ffffn,s=a=>BigInt.toIntN(64,a),u=a=>BigInt.toUIntN(64,a),d=>p=>{{p=$.get_page(p);return new DataView($._sys(`memory`),p)}};{}}}",
            TemplateJit {
                react: self,
                pc: a,
                root:a,
                labels: &BTreeMap::default()
            }
        );
    }
    #[wasm_bindgen(getter)]
    pub fn f(&self) -> u64 {
        u64::MAX
    }
    // #[wasm_bindgen()]
    // pub fn r(&self, a: usize) -> u64 {
    //     if a == 0 {
    //         return 0;
    //     }
    //     unsafe { &mut *self.core.get() }.regs[a - 1]
    // }
    // #[wasm_bindgen]
    // pub fn s(&self, a: usize, x: u64) {
    //     if a == 0 {
    //         return;
    //     }
    //     unsafe { &mut *self.core.get() }.regs[a - 1] = x;
    // }
    async fn _ecall(self) -> Result<JsValue, JsValue> {
        Ok(JsValue::undefined())
    }
    #[wasm_bindgen]
    pub fn ecall(&self) -> Promise {
        let this = self.clone();
        wasm_bindgen_futures::future_to_promise(async move { this._ecall().await })
    }
}
pub struct TemplateJit<'a> {
    react: &'a Reactor,
    pc: u64,
    root: u64,
    labels: &'a BTreeMap<u64, &'a (dyn Display + 'a)>,
}
struct TemplateReg<'a> {
    reg: &'a Reg,
    value: Option<&'a (dyn Display + 'a)>,
}
impl<'a> Display for TemplateReg<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let r = self.reg.0 % 32;
        if r != 0 {
            match self.value.as_deref() {
                None => write!(f, "$.r[`x{r}`]??($.r[`x{r}`]=0n)"),
                Some(a) => write!(f, "$.r[`x{r}`]={a}"),
            }
        } else {
            match self.value.as_deref() {
                None => write!(f, "0n"),
                Some(a) => write!(f, "{a}"),
            }
        }
    }
}
impl<'a> Display for TemplateJit<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if tget(self.react.clone(), self.pc) != JsValue::UNDEFINED {
            return write!(f, "return J({}n);", self.pc);
        }
        let p;
        let i = Inst::decode(
            match self.react.get_page(self.pc) as *mut u32 {
                pp => {
                    p = unsafe { *pp };
                    p
                }
            },
            Xlen::Rv64,
        );
        let mut labels = self.labels.clone();
        match labels.entry(self.pc) {
            std::collections::btree_map::Entry::Vacant(vacant_entry) => {
                let f2 = format!("x{}", self.pc);
                vacant_entry.insert(&f2);
                write!(f, "x{}: for(;;){{const p={0}n;if(d(p).getUInt32(0,true)!={p}){{delete $.p[`{}`];return J(p);}};", self.pc,self.root)?;
                match i {
                    Err(e) => write!(f, "throw $.d(`decoding: {e}`);}}"),
                    Ok((a, b)) => {
                        let next = match b {
                            rv_asm::IsCompressed::Yes => 2,
                            rv_asm::IsCompressed::No => 4,
                        } + self.pc;
                        macro_rules! ops {
                    ($a:expr => [$($arith:ident => $ap:literal $(i $ip:literal)? $(w $bp:literal)? $(iw $iwp:literal)?),*] [$($j:ident => $jp:literal),*] |$x:pat_param|$e:expr) => {
                        paste::paste!{
                        match $a{
                            $(
                                Inst::$arith { dest, src1, src2 } => write!(
                                    f,
                                    "{}",
                                    TemplateReg {
                                        reg: &dest,
                                        value: Some(&format_args!(
                                            $ap,
                                            TemplateReg {
                                                reg: &src1,
                                                value: None
                                            },
                                            TemplateReg {
                                                reg: &src2,
                                                value: None
                                            }
                                        ))
                                    }
                                ),
                                $(Inst::[<$arith i>] { imm, dest, src1 } => match $ip{_=>write!(
                                    f,
                                    "{}",
                                    TemplateReg {
                                        reg: &dest,
                                        value: Some(&format_args!(
                                            $ap,
                                            TemplateReg {
                                                reg: &src1,
                                                value: None
                                            },
                                            &format_args!("{}n",imm.as_i64() as u64)
                                        ))
                                    }
                                )},)?
                                $(Inst::[<$arith W>] { dest, src1, src2 } => write!(
                                    f,
                                    "{}",
                                    TemplateReg {
                                        reg: &dest,
                                        value: Some(&format_args!(
                                            $bp,
                                            TemplateReg {
                                                reg: &src1,
                                                value: None
                                            },
                                            TemplateReg {
                                                reg: &src2,
                                                value: None
                                            }
                                        ))
                                    }
                                ),
                                )?
                                $(Inst::[<$arith iW>] { imm, dest, src1 } => write!(
                                    f,
                                    "{}",
                                    TemplateReg {
                                        reg: &dest,
                                        value: Some(&format_args!(
                                            $iwp,
                                            TemplateReg {
                                                reg: &src1,
                                                value: None
                                            },
                                            &format_args!("{}n",imm.as_i64() as u64)
                                        ))
                                    }
                                ),)?
                            )*
                            $(Inst::$j {src1,src2,offset} => {
                                 write!(f,"if({}){{{}}}else{{{}}};break;}}",
                                    &format_args!(
                                        $jp,
                                        TemplateReg {
                                            reg: &src1,
                                            value: None
                                        },
                                        TemplateReg {
                                            reg: &src2,
                                            value: None
                                        }
                                    ),
                                    TemplateJit{
                                        react: self.react,
                                        labels: &labels,
                                        pc: self.pc.wrapping_add_signed(offset.as_i64() * 2),
                                        root:self.root,
                                    },
                                    TemplateJit{
                                        react: self.react,
                                        labels: &labels,
                                        pc: next,
                                              root:self.root,
                                    }
                                )?;
                                return Ok(());
                            },)*
                            $x => $e
                        }
                        }
                    };
                }
                        ops!(a => [
                            Add => "({}+{})&f" i "" w "(({}&g)+({}&g))&g" iw  "(({}&g)+({}&g))&g",
                            Mul => "({}*{})&f" w "(({}&g)*({}&g))&g",
                            Sub => "({}-{})&f" w "(({}&g)-({}&g))&g",
                            Divu => "({}/{})&f" w "(({}&g)/({}&g))&g",
                            Remu => "({}%{})&f" w "(({}&g)%({}&g))&g",
                            Div => "u(s({})/s({}))&f" w "u(s({}&g)/s({}&g))&g",
                            Rem => "u(s({})%s({}))&f" w "u(s({}&g)%s({}&g))&g",
                            And => "({}&{})&f" i "",
                            Or => "({}|{})&f" i "",
                            Xor => "({}^{})&f" i "",
                            Sll => "({}<<{})&f" i "" w "(({}&g)<<({}&g))&g" iw  "(({}&g)<<({}&g))&g",
                            Srl => "({}>>{})&f" i "" w "(({}&g)>>({}&g))&g" iw  "(({}&g)>>({}&g))&g",
                            Sra => "u(s({})>>s({}))&f" i "" w "u(s({}&g)>>s({}&g))&g" iw  "u(s({}&g)>>s({}&g))&g",
                            Sltu => "(({})<({}))?1n:0n",
                            Slt => "(s({})<s({}))?1n:0n" i ""
                            ]
                            [
                                Beq => "{}==={}",
                                Bne => "{}!=={}",
                                Bltu => "{}<{}",
                                Bgeu => "{}>={}",
                                Blt => "s({})<s({})",
                                Bge => "s({})>=s({})"
                            ] |a|match a{
                                Inst::Jal { offset, dest } => {
                                    write!(f,"{};{};break;}}",TemplateReg{
                                        reg: &dest,
                                        value: Some(&format_args!("{}n",next))
                                    },TemplateJit{react: self.react, labels: &labels, pc: self.pc.wrapping_add_signed(offset.as_i64() * 2),      root:self.root})?;
                                    return Ok(());
                                }
                                Inst::Jalr { offset,base, dest } => {
                                    write!(f,"{};return ()=>J({});}}",TemplateReg{
                                        reg: &dest,
                                        value: Some(&format_args!("{}n",next))
                                    },&format_args!("({}+{})&f",(offset.as_i64() * 2) as u64,TemplateReg{reg: &base,value: None}))?;
                                    return Ok(());
                                }
                                Inst::Lb { offset, dest, base } => write!(f,"{}",TemplateReg{
                                    reg:&dest,
                                    value:Some(&format_args!(
                                        "u(BigInt(d(({}n+{})&f).getInt8(0,true)))",
                                        offset.as_i64() as u64,
                                        TemplateReg{reg:&base,value:None}
                                    ))
                                }),
                                Inst::Lbu { offset, dest, base } => write!(f,"{}",TemplateReg{
                                    reg:&dest,
                                    value:Some(&format_args!(
                                        "BigInt(d(({}n+{})&f).getUInt8(0,true))",
                                        offset.as_i64() as u64,
                                        TemplateReg{reg:&base,value:None}
                                    ))
                                }),
                                Inst::Lh { offset, dest, base } => write!(f,"{}",TemplateReg{
                                    reg:&dest,
                                    value:Some(&format_args!(
                                        "u(BigInt(d(({}n+{})&f).getInt16(0,true)))",
                                        offset.as_i64() as u64,
                                        TemplateReg{reg:&base,value:None}
                                    ))
                                }),
                                Inst::Lhu { offset, dest, base } => write!(f,"{}",TemplateReg{
                                    reg:&dest,
                                    value:Some(&format_args!(
                                        "BigInt(d(({}n+{})&f).getUInt16(0,true))",
                                        offset.as_i64() as u64,
                                        TemplateReg{reg:&base,value:None}
                                    ))
                                }),
                                Inst::Lw { offset, dest, base } => write!(f,"{}",TemplateReg{
                                    reg:&dest,
                                    value:Some(&format_args!(
                                        "u(BigInt(d(({}n+{})&f).getInt32(0,true)))",
                                        offset.as_i64() as u64,
                                        TemplateReg{reg:&base,value:None}
                                    ))
                                }),
                                Inst::Lwu { offset, dest, base } => write!(f,"{}",TemplateReg{
                                    reg:&dest,
                                    value:Some(&format_args!(
                                        "BigInt(d(({}n+{})&f).getUInt32(0,true))",
                                        offset.as_i64() as u64,
                                        TemplateReg{reg:&base,value:None}
                                    ))
                                }),
                                Inst::Ld { offset, dest, base } => write!(f,"{}",TemplateReg{
                                    reg:&dest,
                                    value:Some(&format_args!(
                                        "d(({}n+{})&f).getBigUInt64(0,true)",
                                        offset.as_i64() as u64,
                                        TemplateReg{reg:&base,value:None}
                                    ))
                                }),
                                Inst::Sb { offset, src, base } => write!(f,
                                    "d({}n+{}).setUInt8(0,Number({}&g),true)",
                                    offset.as_i64() as u64,
                                    TemplateReg{reg:&base,value:None},
                                    TemplateReg{reg:&src,value:None}
                                ),
                                Inst::Sh { offset, src, base } => write!(f,
                                    "d({}n+{}).setUInt16(0,Number({}&g),true)",
                                    offset.as_i64() as u64,
                                    TemplateReg{reg:&base,value:None},
                                    TemplateReg{reg:&src,value:None}
                                ),
                                Inst::Sw { offset, src, base } => write!(f,
                                    "d({}n+{}).setUInt32(0,Number({}&g),true)",
                                    offset.as_i64() as u64,
                                    TemplateReg{reg:&base,value:None},
                                    TemplateReg{reg:&src,value:None}
                                ),
                                Inst::Sd { offset, src, base } => write!(f,
                                    "d({}n+{}).setBigUInt64(0,{},true)",
                                    offset.as_i64() as u64,
                                    TemplateReg{reg:&base,value:None},
                                    TemplateReg{reg:&src,value:None}
                                ),
                                Inst::Fence{..} => Ok(()),
                                Inst::Ecall => write!(f,"await $.ecall();"),
                            op => write!(f,"throw $.d(`op:{op}`)"),
                        })?;
                        write!(
                            f,
                            ";{};break;}}",
                            TemplateJit {
                                react: self.react,
                                pc: next,
                                labels: &labels,
                                root: self.root,
                            }
                        )
                    }
                }
            }
            std::collections::btree_map::Entry::Occupied(occupied_entry) => {
                write!(f, "continue {};", occupied_entry.get())
            }
        }
    }
}
