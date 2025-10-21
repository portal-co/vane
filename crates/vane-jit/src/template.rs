use crate::{
    arch::{Riscv, RiscvDisplay},
    *,
};
#[derive(Clone, Copy)]
pub struct Params<'a> {
    pub react: &'a UnsafeCell<Mem>,
    pub trial: &'a (dyn Fn(u64) -> Heat + 'a),
    pub root: u64,
}
pub struct TemplateJit<'a> {
    pub params: Params<'a>,
    pub pc: u64,
    pub labels: &'a BTreeMap<u64, &'a (dyn Display + 'a)>,
}
struct TemplateReg<'a> {
    reg: &'a Reg,
    value: Option<&'a (dyn Display + 'a)>,
}
impl<'a> Display for TemplateReg<'a> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let r = self.reg.0 % 32;
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
impl<'a> RiscvDisplay for TemplateJit<'a> {
    fn Riscv(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // if tget(self.react.clone(), self.pc) != JsValue::UNDEFINED {
        match (self.params.trial)(self.pc) {
            Heat::New => {}
            Heat::Cached => {
                return write!(f, "return J({}n);", self.pc);
            }
        }
        let inst_code;
        let i = Inst::decode(
            match unsafe { &mut *self.params.react.get() }.get_page(self.pc) as *mut u32 {
                inst_code_ptr => {
                    inst_code = unsafe { *inst_code_ptr };
                    inst_code
                }
            },
            Xlen::Rv64,
        );
        let mut labels = self.labels.clone();
        match labels.entry(self.pc) {
            alloc::collections::btree_map::Entry::Vacant(vacant_entry) => {
                let label_name = format!("x{}", self.pc);
                vacant_entry.insert(&label_name);
                write!(
                    f,
                    "{label_name}: for(;;){{const p={}n;if(d(p).getUInt32(0,true)!={inst_code}){{delete $.p[`{}`];return J(p);}};",
                    self.pc, self.params.root
                )?;
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
                                    Riscv(&TemplateJit{
                                     params:self.params,
                                        labels: &labels,
                                        pc: self.pc.wrapping_add_signed(offset.as_i64() * 2),
                                        // root:self.root,
                                    }),
                                    Riscv(&TemplateJit{
                                        params:self.params,
                                        labels: &labels,
                                        pc: next,
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
                                    },Riscv(&TemplateJit{
                                        params:self.params,
                                        labels: &labels,
                                        pc: self.pc.wrapping_add_signed(offset.as_i64() * 2),
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
                                        "BigInt(d(({}n+{})&f).getUInt8(0,true))",
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
                                        "BigInt(d(({}n+{})&f).getUInt16(0,true))",
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
                                        "BigInt(d(({}n+{})&f).getUInt32(0,true))",
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
                                        "d(({}n+{})&f).getBigUInt64(0,true)",
                                        offset.as_i64() as u64,
                                        TemplateReg{
                                            reg:&base,
                                            value:None
                                        }
                                    ))
                                }),
                                Inst::Sb { offset, src, base } => write!(f,
                                    "d({}n+{}).setUInt8(0,Number({}&g),true)",
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
                                    "d({}n+{}).setUInt16(0,Number({}&g),true)",
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
                                    "d({}n+{}).setUInt32(0,Number({}&g),true)",
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
                                    "d({}n+{}).setBigUInt64(0,{},true)",
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
                            op => write!(f,"throw $.d(`op:{op}`)"),
                        })?;
                        write!(
                            f,
                            ";{};break;}}",
                            Riscv(&TemplateJit {
                                params: self.params,
                                pc: next,
                                labels: &labels,
                            })
                        )
                    }
                }
            }
            alloc::collections::btree_map::Entry::Occupied(occupied_entry) => {
                write!(f, "continue {};", occupied_entry.get())
            }
        }
    }
}
