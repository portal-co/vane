use core::array;
use core::fmt::{Display, Formatter};
use core::iter::empty;

use alloc::boxed::Box;
use paste::paste;
use rv_asm::{Inst, Reg, Xlen};

use crate::hint;
use crate::{RiscvDisplay, RiscvWasmJit, TemplateRiscv};

use super::*;

struct TemplateReg<'a, const N: usize = 32> {
    reg: &'a Reg,
    value: Option<&'a (dyn Display + 'a)>,
    n: [(); N],
    flate: &'a (dyn crate::Flate + 'a),
}
impl<'a, const N: usize> Display for TemplateReg<'a, N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let r = self.reg.0 & ((N - 1) & 0xff) as u8;
        if r != 0 {
            match self.value.as_deref() {
                None => write!(f, "(($._r??=$.r)[`x{r}`]??=0n)"),
                Some(a) => write!(f, "(($._r??=$.r)[`x{r}`]={a})"),
            }
        } else {
            match self.value.as_deref() {
                None => write!(f, "0n"),
                Some(a) => write!(f, "{a}"),
            }
        }
    }
}

impl<'b> RiscvWasmJit for TemplateJit<'b> {
    fn Riscv<'a>(
        &'a self,
        ctx: &'a (dyn WasmJitCtx + 'a),
    ) -> impl Iterator<Item = JitOpcode<'a>> + 'a {
        self.jit_wasm(|labels, nd| {
            let inst_code = u32::from_le_bytes(core::array::from_fn(|i| {
                self.params.react.byte(self.pc + i as u64)
            }));
            let i = Inst::decode(inst_code, Xlen::Rv64);
            return Box::new(empty());
        })
    }
}
fn rv_core_js<Target: TemplateJS>(
    this: &TemplateJit<'_>,
    a: Inst,
    next: u64,
    labels: Labels<'_>,
    nd: u32,
    f: &mut Formatter,
    target: Target,
) -> core::fmt::Result {
    let max64 = this.params.flate.flate("max64");
    let max32 = this.params.flate.flate("max32");
    let signed = this.params.flate.flate("signed");
    let unsigned = this.params.flate.flate("unsigned");
    let data = this.params.flate.flate("data");
    macro_rules! j {
        ($jp:literal, $src1:ident, $src2:ident, $offset:ident) => {{
            write!(
                f,
                "if({}){{{}}}else{{{}}};",
                &format_args!(
                    $jp,
                    TemplateReg {
                        flate: this.params.flate,
                        n: [(); 32],
                        reg: &$src1,
                        value: None
                    },
                    TemplateReg {
                        flate: this.params.flate,
                        n: [(); 32],
                        reg: &$src2,
                        value: None
                    }
                ),
                target.template_jit_js(&TemplateJit {
                    params: this.params,
                    labels: &labels,
                    pc: this.pc.wrapping_add_signed(($offset).as_i64() * 2),
                    depth: nd,
                    // root:this.root,
                }),
                target.template_jit_js(&TemplateJit {
                    params: this.params,
                    labels: &labels,
                    pc: next,
                    depth: nd,
                    //   root:this.root,
                })
            )?;
            return Ok(());
        }};
    }
    macro_rules! ops {
            ($a:expr => [$($arith:ident => $ap:literal $(i $ip:literal)? $(w $bp:literal)? $(iw $iwp:literal)?),*] [$($j:ident => $jp:literal),*] |$x:pat_param|$e:expr) => {
                'a: {match paste::paste!{
                    match $a{
                        $(
                            Inst::$arith { dest, src1, src2 } => break 'a write!(
                                f,
                                "{}",
                                TemplateReg{flate: this.params.flate, n: [(); 32],
                                    reg: &dest,
                                    value: Some(&format_args!(
                                        $ap,
                                        TemplateReg{flate: this.params.flate, n: [(); 32],
                                            reg: &src1,
                                            value: None
                                        },
                                        TemplateReg{flate: this.params.flate, n: [(); 32],
                                            reg: &src2,
                                            value: None
                                        }
                                    ))
                                }
                            ),
                            $(Inst::[<$arith i>] { imm, dest, src1 } => break 'a match $ip{_=>write!(
                                f,
                                "{}",
                                TemplateReg{flate: this.params.flate, n: [(); 32],
                                    reg: &dest,
                                    value: Some(&format_args!(
                                        $ap,
                                        TemplateReg{flate: this.params.flate, n: [(); 32],
                                            reg: &src1,
                                            value: None
                                        },
                                        &format_args!("{}n",imm.as_i64() as u64)
                                    ))
                                }
                            )},)?
                            $(Inst::[<$arith W>] { dest, src1, src2 } => break 'a write!(
                                f,
                                "{}",
                                TemplateReg{flate: this.params.flate, n: [(); 32],
                                    reg: &dest,
                                    value: Some(&format_args!(
                                        $bp,
                                        TemplateReg{flate: this.params.flate, n: [(); 32],
                                            reg: &src1,
                                            value: None
                                        },
                                        TemplateReg{flate: this.params.flate, n: [(); 32],
                                            reg: &src2,
                                            value: None
                                        }
                                    ))
                                }
                            ),
                            )?
                            $(Inst::[<$arith iW>] { imm, dest, src1 } => break 'a write!(
                                f,
                                "{}",
                                TemplateReg{flate: this.params.flate, n: [(); 32],
                                    reg: &dest,
                                    value: Some(&format_args!(
                                        $iwp,
                                        TemplateReg{flate: this.params.flate, n: [(); 32],
                                            reg: &src1,
                                            value: None
                                        },
                                        &format_args!("{}n",imm.as_i64() as u64)
                                    ))
                                }
                            ),)?
                        )*
                        $(Inst::$j {src1,src2,offset} => j!($jp,src1,src2,offset),)*
                        a => a
                    }
                }{
                    $x => $e
                }}
            };
        }
    macro_rules! miscop {
        ($a:expr) => {
            match $a {
                a => match a {
                    Inst::Lui { uimm, dest } => {
                        write!(
                            f,
                            "{}",
                            TemplateReg {
                                flate: this.params.flate,
                                n: [(); 32],
                                reg: &dest,
                                value: Some(&format_args!("{}n", uimm.as_u64()))
                            }
                        )
                    }
                    Inst::Auipc { uimm, dest } => {
                        write!(
                            f,
                            "{}",
                            TemplateReg {
                                flate: this.params.flate,
                                n: [(); 32],
                                reg: &dest,
                                value: Some(&format_args!(
                                    "{}n",
                                    uimm.as_u64().wrapping_add(this.pc)
                                ))
                            }
                        )
                    }
                    Inst::Jal { offset, dest } => {
                        write!(
                            f,
                            "{};{};break;}}",
                            TemplateReg {
                                flate: this.params.flate,
                                n: [(); 32],
                                reg: &dest,
                                value: Some(&format_args!("{}n", next))
                            },
                            target.template_jit_js(&TemplateJit {
                                params: this.params,
                                labels: &labels,
                                pc: this.pc.wrapping_add_signed(offset.as_i64() * 2),
                                depth: nd,
                            })
                        )?;
                        return Ok(());
                    }
                    Inst::Jalr { offset, base, dest } => {
                        write!(
                            f,
                            "{};return ()=>J({});}}",
                            TemplateReg {
                                flate: this.params.flate,
                                n: [(); 32],
                                reg: &dest,
                                value: Some(&format_args!("{}n", next))
                            },
                            &format_args!(
                                "({}+{})&{max64}",
                                (offset.as_i64() * 2) as u64,
                                TemplateReg {
                                    flate: this.params.flate,
                                    n: [(); 32],
                                    reg: &base,
                                    value: None
                                }
                            )
                        )?;
                        return Ok(());
                    }
                    Inst::Lb { offset, dest, base } => write!(
                        f,
                        "{}",
                        TemplateReg {
                            flate: this.params.flate,
                            n: [(); 32],
                            reg: &dest,
                            value: Some(&format_args!(
                                "{unsigned}(BigInt({data}(({}n+{})&f).getInt8(0,true)))",
                                offset.as_i64() as u64,
                                TemplateReg {
                                    flate: this.params.flate,
                                    n: [(); 32],
                                    reg: &base,
                                    value: None
                                }
                            ))
                        }
                    ),
                    Inst::Lbu { offset, dest, base } => write!(
                        f,
                        "{}",
                        TemplateReg {
                            flate: this.params.flate,
                            n: [(); 32],
                            reg: &dest,
                            value: Some(&format_args!(
                                "BigInt({data}(({}n+{})&f).getUint8(0,true))",
                                offset.as_i64() as u64,
                                TemplateReg {
                                    flate: this.params.flate,
                                    n: [(); 32],
                                    reg: &base,
                                    value: None
                                }
                            ))
                        }
                    ),
                    Inst::Lh { offset, dest, base } => write!(
                        f,
                        "{}",
                        TemplateReg {
                            flate: this.params.flate,
                            n: [(); 32],
                            reg: &dest,
                            value: Some(&format_args!(
                                "{unsigned}(BigInt({data}(({}n+{})&f).getInt16(0,true)))",
                                offset.as_i64() as u64,
                                TemplateReg {
                                    flate: this.params.flate,
                                    n: [(); 32],
                                    reg: &base,
                                    value: None
                                }
                            ))
                        }
                    ),
                    Inst::Lhu { offset, dest, base } => write!(
                        f,
                        "{}",
                        TemplateReg {
                            flate: this.params.flate,
                            n: [(); 32],
                            reg: &dest,
                            value: Some(&format_args!(
                                "BigInt({data}(({}n+{})&f).getUint16(0,true))",
                                offset.as_i64() as u64,
                                TemplateReg {
                                    flate: this.params.flate,
                                    n: [(); 32],
                                    reg: &base,
                                    value: None
                                }
                            ))
                        }
                    ),
                    Inst::Lw { offset, dest, base } => write!(
                        f,
                        "{}",
                        TemplateReg {
                            flate: this.params.flate,
                            n: [(); 32],
                            reg: &dest,
                            value: Some(&format_args!(
                                "{unsigned}(BigInt({data}(({}n+{})&f).getInt32(0,true)))",
                                offset.as_i64() as u64,
                                TemplateReg {
                                    flate: this.params.flate,
                                    n: [(); 32],
                                    reg: &base,
                                    value: None
                                }
                            ))
                        }
                    ),
                    Inst::Lwu { offset, dest, base } => write!(
                        f,
                        "{}",
                        TemplateReg {
                            flate: this.params.flate,
                            n: [(); 32],
                            reg: &dest,
                            value: Some(&format_args!(
                                "BigInt({data}(({}n+{})&f).getUint32(0,true))",
                                offset.as_i64() as u64,
                                TemplateReg {
                                    flate: this.params.flate,
                                    n: [(); 32],
                                    reg: &base,
                                    value: None
                                }
                            ))
                        }
                    ),
                    Inst::Ld { offset, dest, base } => write!(
                        f,
                        "{}",
                        TemplateReg {
                            flate: this.params.flate,
                            n: [(); 32],
                            reg: &dest,
                            value: Some(&format_args!(
                                "{data}(({}n+{})&f).getBigUint64(0,true)",
                                offset.as_i64() as u64,
                                TemplateReg {
                                    flate: this.params.flate,
                                    n: [(); 32],
                                    reg: &base,
                                    value: None
                                }
                            ))
                        }
                    ),
                    Inst::Sb { offset, src, base } => write!(
                        f,
                        "{data}({}n+{}).setUint8(0,Number({}&{max32}),true)",
                        offset.as_i64() as u64,
                        TemplateReg {
                            flate: this.params.flate,
                            n: [(); 32],
                            reg: &base,
                            value: None
                        },
                        TemplateReg {
                            flate: this.params.flate,
                            n: [(); 32],
                            reg: &src,
                            value: None
                        }
                    ),
                    Inst::Sh { offset, src, base } => write!(
                        f,
                        "{data}({}n+{}).setUint16(0,Number({}&{max32}),true)",
                        offset.as_i64() as u64,
                        TemplateReg {
                            flate: this.params.flate,
                            n: [(); 32],
                            reg: &base,
                            value: None
                        },
                        TemplateReg {
                            flate: this.params.flate,
                            n: [(); 32],
                            reg: &src,
                            value: None
                        }
                    ),
                    Inst::Sw { offset, src, base } => write!(
                        f,
                        "{data}({}n+{}).setUint32(0,Number({}&{max32}),true)",
                        offset.as_i64() as u64,
                        TemplateReg {
                            flate: this.params.flate,
                            n: [(); 32],
                            reg: &base,
                            value: None
                        },
                        TemplateReg {
                            flate: this.params.flate,
                            n: [(); 32],
                            reg: &src,
                            value: None
                        }
                    ),
                    Inst::Sd { offset, src, base } => write!(
                        f,
                        "{data}({}n+{}).setBigUint64(0,{},true)",
                        offset.as_i64() as u64,
                        TemplateReg {
                            flate: this.params.flate,
                            n: [(); 32],
                            reg: &base,
                            value: None
                        },
                        TemplateReg {
                            flate: this.params.flate,
                            n: [(); 32],
                            reg: &src,
                            value: None
                        }
                    ),
                    Inst::Fence { .. } => Ok(()),
                    Inst::Ecall => write!(f, "await $.ecall();"),
                    op => write!(f, "throw new TypeError(`op:{op}`)"),
                },
            }
        };
    }
    ops!(a => [
            Add => "({}+{})&{max64}" i "" w "(({}&{max32})+({}&{max32}))&{max32}" iw  "(({}&{max32})+({}&{max32}))&{max32}",
            Mul => "({}*{})&{max64}" w "(({}&{max32})*({}&{max32}))&{max32}",
            Mulhu => "(({}*{})>>64n)&{max64}",
            Mulhsu => "{unsigned}(({signed}({})*{})>>64n)&{max64}",
            Mulh => "{unsigned}(({signed}({})*{signed}({}))>>64n)&{max64}",
            Sub => "({}-{})&{max64}" w "(({}&{max32})-({}&{max32}))&{max32}",
            Divu => "{1}==0?{unsigned}(-1n):({}/{})&{max64}" w "{1}==0?{unsigned}(-1n):(({}&{max32})/({}&{max32}))&{max32}",
            Remu => "{1}==0?{unsigned}(-1n):({}%{})&{max64}" w "{1}==0?{unsigned}(-1n):(({}&{max32})%({}&{max32}))&{max32}",
            Div => "{1}==0?{unsigned}(-1n):{unsigned}({signed}({})/{signed}({}))&{max64}" w "{1}==0?{unsigned}(-1n):{unsigned}({signed}({}&{max32})/{signed}({}&{max32}))&{max32}",
            Rem => "{1}==0?{unsigned}(-1n):{unsigned}({signed}({})%{signed}({}))&{max64}" w "{1}==0?{unsigned}(-1n):{unsigned}({signed}({}&{max32})%{signed}({}&{max32}))&{max32}",
            And => "({}&{})&{max64}" i "",
            Or => "({}|{})&{max64}" i "",
            Xor => "({}^{})&{max64}" i "",
            Sll => "({}<<{})&{max64}" i "" w "(({}&{max32})<<({}&{max32}))&{max32}" iw  "(({}&{max32})<<({}&{max32}))&{max32}",
            Srl => "({}>>{})&{max64}" i "" w "(({}&{max32})>>({}&{max32}))&{max32}" iw  "(({}&{max32})>>({}&{max32}))&{max32}",
            Sra => "{unsigned}({signed}({})>>{signed}({}))&{max64}" i "" w "{unsigned}({signed}({}&{max32})>>{signed}({}&{max32}))&{max32}" iw  "{unsigned}({signed}({}&{max32})>>{signed}({}&{max32}))&{max32}",
            Sltu => "(({})<({}))?1n:0n",
            Slt => "({signed}({})<{signed}({}))?1n:0n" i ""
            ]
            [
                Beq => "{}==={}",
                Bne => "{}!=={}",
                Bltu => "{}<{}",
                Bgeu => "{}>={}",
                Blt => "{signed}({})<{signed}({})",
                Bge => "{signed}({})>={signed}({})"
            ] |a|miscop!(a))?;
    write!(
        f,
        ";{};",
        target.template_jit_js(&TemplateJit {
            params: this.params,
            pc: next,
            labels: &labels,
            depth: nd,
        })
    )
}
impl<'a> RiscvDisplay for TemplateJit<'a> {
    fn Riscv(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // if tget(self.react.clone(), self.pc) != JsValue::UNDEFINED {
        return self.jit_js(f, |f,_label_name,labels,nd|{
               let d =  self.params.flate.flate("data");
            let inst_code = u32::from_le_bytes(core::array::from_fn(|i| self.params.react.byte(self.pc + i as u64)));
            let i = Inst::decode(inst_code, Xlen::Rv64);
            write!(
                f,
                "const p={}n;if({d}(p).getUint32(0,true)!={inst_code}){{delete $.p[`{}`];return J(p);}};",
                self.pc, self.params.root
            )?;
            match i {
                Err(e) => write!(f, "throw new TypeError(`decoding: {e}`);"),
                Ok((a, b)) => {
                    // Emit HINT logging if test_mode is enabled
                    if self.params.flags.test_mode {
                        if let Some(hint_value) = hint::detect_test_marker(&a) {
                            write!(
                                f,
                                "console.log(`[HINT] PC=0x{:x}: Test case {}`);",
                                self.pc, hint_value
                            )?;
                        }
                    }
                    let next = match b {
                        rv_asm::IsCompressed::Yes => 2,
                        rv_asm::IsCompressed::No => 4,
                    } + self.pc;
                    rv_core_js(self, a, next, labels, nd, f,TemplateRiscv)?;
                    Ok(())
                }
            }
        });
    }
}
