use alloc::vec::Vec;
use wasmparser::Operator;

use crate::arch::RiscvWasmJit;

use super::*;
pub struct TemplateRiscv;
impl TemplateJS for TemplateRiscv {
    type Ty<'a> = Riscv<'a>;

    fn template_jit_js<'a>(&self, j: &'a TemplateJit<'_>) -> Self::Ty<'a> {
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
        labels: Labels<'_>,
        nd: u32,
        f: &mut Formatter,
        target: Target,
    ) -> core::fmt::Result {
        let f2 = self.params.flate.flate("max64");
        let g = self.params.flate.flate("max32");
        let s = self.params.flate.flate("signed");
        let u = self.params.flate.flate("unsigned");
        let d = self.params.flate.flate("data");
        macro_rules! ops {
                    ($a:expr => [$($arith:ident => $ap:literal $(i $ip:literal)? $(w $bp:literal)? $(iw $iwp:literal)?),*] [$($j:ident => $jp:literal),*] |$x:pat_param|$e:expr) => {
                        paste::paste!{
                        match $a{
                            $(
                                Inst::$arith { dest, src1, src2 } => write!(
                                    f,
                                    "{}",
                                  TemplateReg{flate: self.params.flate, n: [(); 32],
                                        reg: &dest,
                                        value: Some(&format_args!(
                                            $ap,
                                          TemplateReg{flate: self.params.flate, n: [(); 32],
                                                reg: &src1,
                                                value: None
                                            },
                                          TemplateReg{flate: self.params.flate, n: [(); 32],
                                                reg: &src2,
                                                value: None
                                            }
                                        ))
                                    }
                                ),
                                $(Inst::[<$arith i>] { imm, dest, src1 } => match $ip{_=>write!(
                                    f,
                                    "{}",
                                  TemplateReg{flate: self.params.flate, n: [(); 32],
                                        reg: &dest,
                                        value: Some(&format_args!(
                                            $ap,
                                          TemplateReg{flate: self.params.flate, n: [(); 32],
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
                                  TemplateReg{flate: self.params.flate, n: [(); 32],
                                        reg: &dest,
                                        value: Some(&format_args!(
                                            $bp,
                                          TemplateReg{flate: self.params.flate, n: [(); 32],
                                                reg: &src1,
                                                value: None
                                            },
                                          TemplateReg{flate: self.params.flate, n: [(); 32],
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
                                  TemplateReg{flate: self.params.flate, n: [(); 32],
                                        reg: &dest,
                                        value: Some(&format_args!(
                                            $iwp,
                                          TemplateReg{flate: self.params.flate, n: [(); 32],
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
                                      TemplateReg{flate: self.params.flate, n: [(); 32],
                                            reg: &src1,
                                            value: None
                                        },
                                      TemplateReg{flate: self.params.flate, n: [(); 32],
                                            reg: &src2,
                                            value: None
                                        }
                                    ),
                                    target.template_jit_js(&TemplateJit{
                                     params:self.params,
                                        labels: &labels,
                                        pc: self.pc.wrapping_add_signed(offset.as_i64() * 2),
                                        depth:nd,
                                        // root:self.root,
                                    }),
                                    target.template_jit_js(&TemplateJit{
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
            Add => "({}+{})&{f2}" i "" w "(({}&{g})+({}&{g}))&{g}" iw  "(({}&{g})+({}&{g}))&{g}",
            Mul => "({}*{})&{f2}" w "(({}&{g})*({}&{g}))&{g}",
            Mulhu => "(({}*{})>>64n)&{f2}",
            Mulhsu => "{u}(({s}({})*{})>>64n)&{f2}",
            Mulh => "{u}(({s}({})*{s}({}))>>64n)&{f2}",
            Sub => "({}-{})&{f2}" w "(({}&{g})-({}&{g}))&{g}",
            Divu => "{1}==0?{u}(-1n):({}/{})&{f2}" w "{1}==0?{u}(-1n):(({}&{g})/({}&{g}))&{g}",
            Remu => "{1}==0?{u}(-1n):({}%{})&{f2}" w "{1}==0?{u}(-1n):(({}&{g})%({}&{g}))&{g}",
            Div => "{1}==0?{u}(-1n):{u}({s}({})/{s}({}))&{f2}" w "{1}==0?{u}(-1n):{u}({s}({}&{g})/{s}({}&{g}))&{g}",
            Rem => "{1}==0?{u}(-1n):{u}({s}({})%{s}({}))&{f2}" w "{1}==0?{u}(-1n):{u}({s}({}&{g})%{s}({}&{g}))&{g}",
            And => "({}&{})&{f2}" i "",
            Or => "({}|{})&{f2}" i "",
            Xor => "({}^{})&{f2}" i "",
            Sll => "({}<<{})&{f2}" i "" w "(({}&{g})<<({}&{g}))&{g}" iw  "(({}&{g})<<({}&{g}))&{g}",
            Srl => "({}>>{})&{f2}" i "" w "(({}&{g})>>({}&{g}))&{g}" iw  "(({}&{g})>>({}&{g}))&{g}",
            Sra => "{u}({s}({})>>{s}({}))&{f2}" i "" w "{u}({s}({}&{g})>>{s}({}&{g}))&{g}" iw  "{u}({s}({}&{g})>>{s}({}&{g}))&{g}",
            Sltu => "(({})<({}))?1n:0n",
            Slt => "({s}({})<{s}({}))?1n:0n" i ""
            ]
            [
                Beq => "{}==={}",
                Bne => "{}!=={}",
                Bltu => "{}<{}",
                Bgeu => "{}>={}",
                Blt => "{s}({})<{s}({})",
                Bge => "{s}({})>={s}({})"
            ] |a|match a{
                Inst::Lui { uimm, dest } => {
                    write!(f,"{}",TemplateReg{flate: self.params.flate, n: [(); 32],reg:&dest,value:Some(&format_args!("{}n",uimm.as_u64()))})
                }
                 Inst::Auipc { uimm, dest } => {
                    write!(f,"{}",TemplateReg{flate: self.params.flate, n: [(); 32],reg:&dest,value:Some(&format_args!("{}n",uimm.as_u64().wrapping_add(self.pc)))})
                }
                Inst::Jal { offset, dest } => {
                    write!(f,"{};{};break;}}",TemplateReg{flate: self.params.flate, n: [(); 32],
                        reg: &dest,
                        value: Some(&format_args!("{}n",next))
                    },target.template_jit_js(&TemplateJit{
                        params:self.params,
                        labels: &labels,
                        pc: self.pc.wrapping_add_signed(offset.as_i64() * 2),
                         depth:nd,
                    }))?;
                    return Ok(());
                }
                Inst::Jalr { offset,base, dest } => {
                    write!(f,"{};return ()=>J({});}}",TemplateReg{flate: self.params.flate, n: [(); 32],
                        reg: &dest,
                        value: Some(&format_args!("{}n",next))
                    },&format_args!("({}+{})&{f2}",(offset.as_i64() * 2) as u64,TemplateReg{flate: self.params.flate, n: [(); 32],
                        reg: &base,
                        value: None
                    }))?;
                    return Ok(());
                }
                Inst::Lb { offset, dest, base } => write!(f,"{}",TemplateReg{flate: self.params.flate, n: [(); 32],
                    reg:&dest,
                    value:Some(&format_args!(
                        "{u}(BigInt({d}(({}n+{})&f).getInt8(0,true)))",
                        offset.as_i64() as u64,
                      TemplateReg{flate: self.params.flate, n: [(); 32],
                            reg:&base,
                            value:None
                        }
                    ))
                }),
                Inst::Lbu { offset, dest, base } => write!(f,"{}",TemplateReg{flate: self.params.flate, n: [(); 32],
                    reg:&dest,
                    value:Some(&format_args!(
                        "BigInt({d}(({}n+{})&f).getUint8(0,true))",
                        offset.as_i64() as u64,
                      TemplateReg{flate: self.params.flate, n: [(); 32],
                            reg:&base,
                            value:None
                        }
                    ))
                }),
                Inst::Lh { offset, dest, base } => write!(f,"{}",TemplateReg{flate: self.params.flate, n: [(); 32],
                    reg:&dest,
                    value:Some(&format_args!(
                        "{u}(BigInt({d}(({}n+{})&f).getInt16(0,true)))",
                        offset.as_i64() as u64,
                      TemplateReg{flate: self.params.flate, n: [(); 32],
                            reg:&base,
                            value:None
                        }
                    ))
                }),
                Inst::Lhu { offset, dest, base } => write!(f,"{}",TemplateReg{flate: self.params.flate, n: [(); 32],
                    reg:&dest,
                    value:Some(&format_args!(
                        "BigInt({d}(({}n+{})&f).getUint16(0,true))",
                        offset.as_i64() as u64,
                      TemplateReg{flate: self.params.flate, n: [(); 32],
                            reg:&base,
                            value:None
                        }
                    ))
                }),
                Inst::Lw { offset, dest, base } => write!(f,"{}",TemplateReg{flate: self.params.flate, n: [(); 32],
                    reg:&dest,
                    value:Some(&format_args!(
                        "{u}(BigInt({d}(({}n+{})&f).getInt32(0,true)))",
                        offset.as_i64() as u64,
                      TemplateReg{flate: self.params.flate, n: [(); 32],
                            reg:&base,
                            value:None
                        }
                    ))
                }),
                Inst::Lwu { offset, dest, base } => write!(f,"{}",TemplateReg{flate: self.params.flate, n: [(); 32],
                    reg:&dest,
                    value:Some(&format_args!(
                        "BigInt({d}(({}n+{})&f).getUint32(0,true))",
                        offset.as_i64() as u64,
                      TemplateReg{flate: self.params.flate, n: [(); 32],
                            reg:&base,
                            value:None
                        }
                    ))
                }),
                Inst::Ld { offset, dest, base } => write!(f,"{}",TemplateReg{flate: self.params.flate, n: [(); 32],
                    reg:&dest,
                    value:Some(&format_args!(
                        "{d}(({}n+{})&f).getBigUint64(0,true)",
                        offset.as_i64() as u64,
                      TemplateReg{flate: self.params.flate, n: [(); 32],
                            reg:&base,
                            value:None
                        }
                    ))
                }),
                Inst::Sb { offset, src, base } => write!(f,
                    "{d}({}n+{}).setUint8(0,Number({}&{g}),true)",
                    offset.as_i64() as u64,
                  TemplateReg{flate: self.params.flate, n: [(); 32],
                        reg:&base,
                        value:None
                    },
                  TemplateReg{flate: self.params.flate, n: [(); 32],
                        reg:&src,
                        value:None
                    }
                ),
                Inst::Sh { offset, src, base } => write!(f,
                    "{d}({}n+{}).setUint16(0,Number({}&{g}),true)",
                    offset.as_i64() as u64,
                  TemplateReg{flate: self.params.flate, n: [(); 32],
                        reg:&base,
                        value:None
                    },
                  TemplateReg{flate: self.params.flate, n: [(); 32],
                        reg:&src,
                        value:None
                    }
                ),
                Inst::Sw { offset, src, base } => write!(f,
                    "{d}({}n+{}).setUint32(0,Number({}&{g}),true)",
                    offset.as_i64() as u64,
                  TemplateReg{flate: self.params.flate, n: [(); 32],
                        reg:&base,
                        value:None
                    },
                  TemplateReg{flate: self.params.flate, n: [(); 32],
                        reg:&src,
                        value:None
                    }
                ),
                Inst::Sd { offset, src, base } => write!(f,
                    "{d}({}n+{}).setBigUint64(0,{},true)",
                    offset.as_i64() as u64,
                  TemplateReg{flate: self.params.flate, n: [(); 32],
                        reg:&base,
                        value:None
                    },
                  TemplateReg{flate: self.params.flate, n: [(); 32],
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
            target.template_jit_js(&TemplateJit {
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
               let d =  self.params.flate.flate("data");
            let mut i = self.params.react.bytes(self.pc);
            let inst_code = u32::from_le_bytes(array::from_fn(|_| i.next().unwrap()));
            let i = Inst::decode(inst_code, Xlen::Rv64);
            write!(
                f,
                "const p={}n;if({d}(p).getUint32(0,true)!={inst_code}){{delete $.p[`{}`];return J(p);}};",
                self.pc, self.params.root
            )?;
            match i {
                Err(e) => write!(f, "throw new TypeError(`decoding: {e}`);"),
                Ok((a, b)) => {
                    let next = match b {
                        rv_asm::IsCompressed::Yes => 2,
                        rv_asm::IsCompressed::No => 4,
                    } + self.pc;
                    self.rv_core_js(a, next, labels, nd, f,TemplateRiscv)?;
                    Ok(())
                }
            }
        });
    }
}
