//! `StackOp` serialization (PLAN_VIRTIO_WEVAL): so a hot trace cache can be
//! stored in / read from a snapshotted WASM module. The interpreter serializes
//! its trace cache (pc → StackOp trace) into a custom section before Wizer
//! snapshots; the patcher reads it back, recompiles, and fills the slots.

use crate::snapshot_ser::{Reader, SerError, Writer};
use alloc::vec::Vec;
use vane_arch::{FloatBinOp, FloatUnaryOp, FloatWidth, MemWidth, StackOp, StateSlot};

fn width(w: &FloatWidth) -> u8 {
    match w {
        FloatWidth::F32 => 0,
        FloatWidth::F64 => 1,
    }
}
fn unwidth(r: &mut Reader) -> Result<FloatWidth, SerError> {
    Ok(match r.u8()? {
        0 => FloatWidth::F32,
        1 => FloatWidth::F64,
        _ => return Err(SerError::Invalid("FloatWidth")),
    })
}
fn binop(o: &FloatBinOp) -> u8 {
    match o {
        FloatBinOp::Add => 0,
        FloatBinOp::Sub => 1,
        FloatBinOp::Mul => 2,
        FloatBinOp::Div => 3,
        FloatBinOp::Min => 4,
        FloatBinOp::Max => 5,
    }
}
fn unbinop(r: &mut Reader) -> Result<FloatBinOp, SerError> {
    Ok(match r.u8()? {
        0 => FloatBinOp::Add,
        1 => FloatBinOp::Sub,
        2 => FloatBinOp::Mul,
        3 => FloatBinOp::Div,
        4 => FloatBinOp::Min,
        5 => FloatBinOp::Max,
        _ => return Err(SerError::Invalid("FloatBinOp")),
    })
}
fn unop(o: &FloatUnaryOp) -> u8 {
    match o {
        FloatUnaryOp::Abs => 0,
        FloatUnaryOp::Neg => 1,
        FloatUnaryOp::Sqrt => 2,
    }
}
fn ununop(r: &mut Reader) -> Result<FloatUnaryOp, SerError> {
    Ok(match r.u8()? {
        0 => FloatUnaryOp::Abs,
        1 => FloatUnaryOp::Neg,
        2 => FloatUnaryOp::Sqrt,
        _ => return Err(SerError::Invalid("FloatUnaryOp")),
    })
}
fn memwidth(w: &MemWidth) -> u8 {
    match w {
        MemWidth::B1 => 1,
        MemWidth::B2 => 2,
        MemWidth::B4 => 4,
        MemWidth::B8 => 8,
    }
}
fn unmemwidth(r: &mut Reader) -> Result<MemWidth, SerError> {
    Ok(match r.u8()? {
        1 => MemWidth::B1,
        2 => MemWidth::B2,
        4 => MemWidth::B4,
        8 => MemWidth::B8,
        _ => return Err(SerError::Invalid("MemWidth")),
    })
}

/// Encode one StackOp.
pub fn encode_op(op: &StackOp, w: &mut Writer) {
    use StackOp::*;
    match op {
        LoopBegin { label } => {
            w.u8(0);
            w.str(label);
        }
        LoopEnd => w.u8(1),
        Br {
            relative_depth,
            label,
        } => {
            w.u8(2);
            w.u32(*relative_depth);
            w.str(label);
        }
        IfBegin => w.u8(3),
        IfElse => w.u8(4),
        IfEnd => w.u8(5),
        PushImm(v) => {
            w.u8(6);
            w.u64(*v);
        }
        LoadReg(r0) => {
            w.u8(7);
            w.u8(*r0);
        }
        StoreReg(r0) => {
            w.u8(8);
            w.u8(*r0);
        }
        Drop => w.u8(9),
        LoadState(s) => {
            w.u8(10);
            w.str(s.as_str());
        }
        StoreState(s) => {
            w.u8(11);
            w.str(s.as_str());
        }
        Add64 => w.u8(12),
        Sub64 => w.u8(13),
        Mul64 => w.u8(14),
        DivU64 => w.u8(15),
        DivS64 => w.u8(16),
        RemU64 => w.u8(17),
        RemS64 => w.u8(18),
        And64 => w.u8(19),
        Or64 => w.u8(20),
        Xor64 => w.u8(21),
        Shl64 => w.u8(22),
        ShrU64 => w.u8(23),
        ShrS64 => w.u8(24),
        MulHU64 => w.u8(25),
        MulHSU64 => w.u8(26),
        MulHS64 => w.u8(27),
        Add32 => w.u8(28),
        Sub32 => w.u8(29),
        Mul32 => w.u8(30),
        DivU32 => w.u8(31),
        DivS32 => w.u8(32),
        RemU32 => w.u8(33),
        RemS32 => w.u8(34),
        Shl32 => w.u8(35),
        ShrU32 => w.u8(36),
        ShrS32 => w.u8(37),
        Trunc32 => w.u8(38),
        SignExt32 => w.u8(39),
        AsSigned64 => w.u8(40),
        AsUnsigned64 => w.u8(41),
        Eq64 => w.u8(42),
        Ne64 => w.u8(43),
        LtU64 => w.u8(44),
        LtS64 => w.u8(45),
        GeU64 => w.u8(46),
        GeS64 => w.u8(47),
        FloatUnary { width: fw, op } => {
            w.u8(48);
            w.u8(width(fw));
            w.u8(unop(op));
        }
        FloatBinary { width: fw, op } => {
            w.u8(49);
            w.u8(width(fw));
            w.u8(binop(op));
        }
        FloatLt { width: fw } => {
            w.u8(50);
            w.u8(width(fw));
        }
        FloatEq { width: fw } => {
            w.u8(51);
            w.u8(width(fw));
        }
        FloatUnordered { width: fw } => {
            w.u8(52);
            w.u8(width(fw));
        }
        IntToFloat {
            width: fw,
            signed,
            int_width,
        } => {
            w.u8(53);
            w.u8(width(fw));
            w.bool(*signed);
            w.u8(*int_width);
        }
        FloatToInt {
            width: fw,
            signed,
            int_width,
        } => {
            w.u8(54);
            w.u8(width(fw));
            w.bool(*signed);
            w.u8(*int_width);
        }
        FloatConvert { from, to } => {
            w.u8(55);
            w.u8(width(from));
            w.u8(width(to));
        }
        LoadMem { width: mw, signed } => {
            w.u8(56);
            w.u8(memwidth(mw));
            w.bool(*signed);
        }
        StoreMem { width: mw } => {
            w.u8(57);
            w.u8(memwidth(mw));
        }
        Ecall => w.u8(58),
        Trap(m) => {
            w.u8(59);
            w.str(m);
        }
        TailCall => w.u8(60),
        Log(m) => {
            w.u8(61);
            w.str(m);
        }
        CheckCode {
            addr,
            expected,
            miss_label,
        } => {
            w.u8(62);
            w.u64(*addr);
            w.blob(expected);
            w.str(miss_label);
        }
    }
}

/// Decode one StackOp.
pub fn decode_op(r: &mut Reader) -> Result<StackOp, SerError> {
    use StackOp::*;
    Ok(match r.u8()? {
        0 => LoopBegin { label: r.str()? },
        1 => LoopEnd,
        2 => Br {
            relative_depth: r.u32()?,
            label: r.str()?,
        },
        3 => IfBegin,
        4 => IfElse,
        5 => IfEnd,
        6 => PushImm(r.u64()?),
        7 => LoadReg(r.u8()?),
        8 => StoreReg(r.u8()?),
        9 => Drop,
        10 => LoadState(StateSlot::new(r.str()?)),
        11 => StoreState(StateSlot::new(r.str()?)),
        12 => Add64,
        13 => Sub64,
        14 => Mul64,
        15 => DivU64,
        16 => DivS64,
        17 => RemU64,
        18 => RemS64,
        19 => And64,
        20 => Or64,
        21 => Xor64,
        22 => Shl64,
        23 => ShrU64,
        24 => ShrS64,
        25 => MulHU64,
        26 => MulHSU64,
        27 => MulHS64,
        28 => Add32,
        29 => Sub32,
        30 => Mul32,
        31 => DivU32,
        32 => DivS32,
        33 => RemU32,
        34 => RemS32,
        35 => Shl32,
        36 => ShrU32,
        37 => ShrS32,
        38 => Trunc32,
        39 => SignExt32,
        40 => AsSigned64,
        41 => AsUnsigned64,
        42 => Eq64,
        43 => Ne64,
        44 => LtU64,
        45 => LtS64,
        46 => GeU64,
        47 => GeS64,
        48 => FloatUnary {
            width: unwidth(r)?,
            op: ununop(r)?,
        },
        49 => FloatBinary {
            width: unwidth(r)?,
            op: unbinop(r)?,
        },
        50 => FloatLt { width: unwidth(r)? },
        51 => FloatEq { width: unwidth(r)? },
        52 => FloatUnordered { width: unwidth(r)? },
        53 => IntToFloat {
            width: unwidth(r)?,
            signed: r.bool()?,
            int_width: r.u8()?,
        },
        54 => FloatToInt {
            width: unwidth(r)?,
            signed: r.bool()?,
            int_width: r.u8()?,
        },
        55 => FloatConvert {
            from: unwidth(r)?,
            to: unwidth(r)?,
        },
        56 => LoadMem {
            width: unmemwidth(r)?,
            signed: r.bool()?,
        },
        57 => StoreMem {
            width: unmemwidth(r)?,
        },
        58 => Ecall,
        59 => Trap(r.str()?),
        60 => TailCall,
        61 => Log(r.str()?),
        62 => CheckCode {
            addr: r.u64()?,
            expected: r.blob()?.into(),
            miss_label: r.str()?,
        },
        _ => return Err(SerError::Invalid("StackOp discriminant")),
    })
}

/// Serialize a trace cache: a set of (guest pc, StackOp trace) pairs.
pub fn serialize_trace_cache(traces: &[(u64, Vec<StackOp>)]) -> Vec<u8> {
    let mut w = Writer::new();
    w.u32(traces.len() as u32);
    for (pc, ops) in traces {
        w.u64(*pc);
        w.u32(ops.len() as u32);
        for op in ops {
            encode_op(op, &mut w);
        }
    }
    w.b
}

/// Deserialize a trace cache written by [`serialize_trace_cache`].
pub fn deserialize_trace_cache(bytes: &[u8]) -> Result<Vec<(u64, Vec<StackOp>)>, SerError> {
    let mut r = Reader::new(bytes);
    let count = r.u32()?;
    let mut out = Vec::new();
    for _ in 0..count {
        let pc = r.u64()?;
        let n = r.u32()?;
        let mut ops = Vec::new();
        for _ in 0..n {
            ops.push(decode_op(&mut r)?);
        }
        out.push((pc, ops));
    }
    Ok(out)
}
