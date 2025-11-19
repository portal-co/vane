use alloc::vec::Vec;
use wasmparser::Operator;

use crate::arch::RiscvWasmJit;

use super::*;
pub struct TemplateRiscv;
impl TemplateJS for TemplateRiscv {
    type Ty<'a> = Riscv<'a>;

    fn template_jit_js<'a>(j: &'a TemplateJit<'_>) -> Self::Ty<'a> {
        Riscv(j)
    }
}
impl<'b> RiscvWasmJit for TemplateJit<'b> {
    fn Riscv<'a>(&'a self) -> Box<dyn Iterator<Item = JitOpcode<'a>> + 'a> {
        self.jit_wasm(|v, labels, nd| {
            let mut i = self.params.react.bytes(self.pc);
            let inst_code = u32::from_le_bytes(array::from_fn(|_| i.next().unwrap()));
            let i = Inst::decode(inst_code, Xlen::Rv64);
        })
    }
}
impl<'a> TemplateJit<'a> {
    fn rv_core_js<Target: TemplateJS>(
        &self,
        a: Inst,
        next: u64,
        labels: BTreeMap<u64, (&(dyn Display + '_), u32)>,
        nd: u32,
        f: &mut Formatter,
    ) -> core::fmt::Result {
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
                                 write!(f,"if({}){{{}}}else{{{}}};",
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
                                    Target::template_jit_js(&TemplateJit{
                                     params:self.params,
                                        labels: &labels,
                                        pc: self.pc.wrapping_add_signed(offset.as_i64() * 2),
                                        depth:nd,
                                        // root:self.root,
                                    }),
                                    Target::template_jit_js(&TemplateJit{
                                        params:self.params,
                                        labels: &labels,
                                        pc: next,
                                         depth:nd,
                                            //   root:self.root,
                                    })
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
            Mulhu => "(({}*{})>>64n)&f",
            Mulhsu => "u((s({})*{})>>64n)&f",
            Mulh => "u((s({})*s({}))>>64n)&f",
            Sub => "({}-{})&f" w "(({}&g)-({}&g))&g",
            Divu => "{1}==0?u(-1n):({}/{})&f" w "{1}==0?u(-1n):(({}&g)/({}&g))&g",
            Remu => "{1}==0?u(-1n):({}%{})&f" w "{1}==0?u(-1n):(({}&g)%({}&g))&g",
            Div => "{1}==0?u(-1n):u(s({})/s({}))&f" w "{1}==0?u(-1n):u(s({}&g)/s({}&g))&g",
            Rem => "{1}==0?u(-1n):u(s({})%s({}))&f" w "{1}==0?u(-1n):u(s({}&g)%s({}&g))&g",
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
                Inst::Lui { uimm, dest } => {
                    write!(f,"{}",TemplateReg{reg:&dest,value:Some(&format_args!("{}n",uimm.as_u64()))})
                }
                 Inst::Auipc { uimm, dest } => {
                    write!(f,"{}",TemplateReg{reg:&dest,value:Some(&format_args!("{}n",uimm.as_u64().wrapping_add(self.pc)))})
                }
                Inst::Jal { offset, dest } => {
                    write!(f,"{};{};break;}}",TemplateReg{
                        reg: &dest,
                        value: Some(&format_args!("{}n",next))
                    },Target::template_jit_js(&TemplateJit{
                        params:self.params,
                        labels: &labels,
                        pc: self.pc.wrapping_add_signed(offset.as_i64() * 2),
                         depth:nd,
                    }))?;
                    return Ok(());
                }
                Inst::Jalr { offset,base, dest } => {
                    write!(f,"{};return ()=>J({});}}",TemplateReg{
                        reg: &dest,
                        value: Some(&format_args!("{}n",next))
                    },&format_args!("({}+{})&f",(offset.as_i64() * 2) as u64,TemplateReg{
                        reg: &base,
                        value: None
                    }))?;
                    return Ok(());
                }
                Inst::Lb { offset, dest, base } => write!(f,"{}",TemplateReg{
                    reg:&dest,
                    value:Some(&format_args!(
                        "u(BigInt(d(({}n+{})&f).getInt8(0,true)))",
                        offset.as_i64() as u64,
                        TemplateReg{
                            reg:&base,
                            value:None
                        }
                    ))
                }),
                Inst::Lbu { offset, dest, base } => write!(f,"{}",TemplateReg{
                    reg:&dest,
                    value:Some(&format_args!(
                        "BigInt(d(({}n+{})&f).getUint8(0,true))",
                        offset.as_i64() as u64,
                        TemplateReg{
                            reg:&base,
                            value:None
                        }
                    ))
                }),
                Inst::Lh { offset, dest, base } => write!(f,"{}",TemplateReg{
                    reg:&dest,
                    value:Some(&format_args!(
                        "u(BigInt(d(({}n+{})&f).getInt16(0,true)))",
                        offset.as_i64() as u64,
                        TemplateReg{
                            reg:&base,
                            value:None
                        }
                    ))
                }),
                Inst::Lhu { offset, dest, base } => write!(f,"{}",TemplateReg{
                    reg:&dest,
                    value:Some(&format_args!(
                        "BigInt(d(({}n+{})&f).getUint16(0,true))",
                        offset.as_i64() as u64,
                        TemplateReg{
                            reg:&base,
                            value:None
                        }
                    ))
                }),
                Inst::Lw { offset, dest, base } => write!(f,"{}",TemplateReg{
                    reg:&dest,
                    value:Some(&format_args!(
                        "u(BigInt(d(({}n+{})&f).getInt32(0,true)))",
                        offset.as_i64() as u64,
                        TemplateReg{
                            reg:&base,
                            value:None
                        }
                    ))
                }),
                Inst::Lwu { offset, dest, base } => write!(f,"{}",TemplateReg{
                    reg:&dest,
                    value:Some(&format_args!(
                        "BigInt(d(({}n+{})&f).getUint32(0,true))",
                        offset.as_i64() as u64,
                        TemplateReg{
                            reg:&base,
                            value:None
                        }
                    ))
                }),
                Inst::Ld { offset, dest, base } => write!(f,"{}",TemplateReg{
                    reg:&dest,
                    value:Some(&format_args!(
                        "d(({}n+{})&f).getBigUint64(0,true)",
                        offset.as_i64() as u64,
                        TemplateReg{
                            reg:&base,
                            value:None
                        }
                    ))
                }),
                Inst::Sb { offset, src, base } => write!(f,
                    "d({}n+{}).setUint8(0,Number({}&g),true)",
                    offset.as_i64() as u64,
                    TemplateReg{
                        reg:&base,
                        value:None
                    },
                    TemplateReg{
                        reg:&src,
                        value:None
                    }
                ),
                Inst::Sh { offset, src, base } => write!(f,
                    "d({}n+{}).setUint16(0,Number({}&g),true)",
                    offset.as_i64() as u64,
                    TemplateReg{
                        reg:&base,
                        value:None
                    },
                    TemplateReg{
                        reg:&src,
                        value:None
                    }
                ),
                Inst::Sw { offset, src, base } => write!(f,
                    "d({}n+{}).setUint32(0,Number({}&g),true)",
                    offset.as_i64() as u64,
                    TemplateReg{
                        reg:&base,
                        value:None
                    },
                    TemplateReg{
                        reg:&src,
                        value:None
                    }
                ),
                Inst::Sd { offset, src, base } => write!(f,
                    "d({}n+{}).setBigUint64(0,{},true)",
                    offset.as_i64() as u64,
                    TemplateReg{
                        reg:&base,
                        value:None
                    },
                    TemplateReg{
                        reg:&src,
                        value:None
                    }
                ),
                Inst::Fence{..} => Ok(()),
                Inst::Ecall => write!(f,"await $.ecall();"),
            op => write!(f,"throw new TypeError(`op:{op}`)"),
        })?;
        write!(
            f,
            ";{};",
            Target::template_jit_js(&TemplateJit {
                params: self.params,
                pc: next,
                labels: &labels,
                depth: nd,
            })
        )
    }
}
impl<'a> RiscvDisplay for TemplateJit<'a> {
    fn Riscv(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // if tget(self.react.clone(), self.pc) != JsValue::UNDEFINED {
        return self.jit_js(f, |f,label_name,labels,nd|{
            let mut i = self.params.react.bytes(self.pc);
            let inst_code = u32::from_le_bytes(array::from_fn(|_| i.next().unwrap()));
            let i = Inst::decode(inst_code, Xlen::Rv64);
            write!(
                f,
                "const p={}n;if(d(p).getUint32(0,true)!={inst_code}){{delete $.p[`{}`];return J(p);}};",
                self.pc, self.params.root
            )?;
            match i {
                Err(e) => write!(f, "throw new TypeError(`decoding: {e}`);"),
                Ok((a, b)) => {
                    let next = match b {
                        rv_asm::IsCompressed::Yes => 2,
                        rv_asm::IsCompressed::No => 4,
                    } + self.pc;
                    self.rv_core_js::<TemplateRiscv>(a, next, labels, nd, f)?;
                    Ok(())
                }
            }
        });
    }
}
