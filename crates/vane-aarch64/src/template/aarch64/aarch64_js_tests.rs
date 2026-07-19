use super::*;
use alloc::{collections::BTreeMap, format};
use core::fmt::{self, Formatter};
use vane_arch::DebugFlate;

struct HostMem(BTreeMap<u64, u8>);
impl vane_arch::JitCtx for HostMem {
    fn bytes(&self, addr: u64) -> alloc::boxed::Box<dyn Iterator<Item = u8> + '_> {
        Box::new((addr..).map(|addr| self.0.get(&addr).copied().unwrap_or(0)))
    }
}

struct Render<'a>(&'a TemplateJit<'a>);
impl fmt::Display for Render<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Aarch64Display::Aarch64(self.0, f)
    }
}

#[test]
fn scalar_fp_jit_source_is_valid_node_javascript() {
    // fcmp d0,d1; fcsel d2,d3,d4,eq; fmadd d5,d6,d7,d8; fmov s9,#1;
    // fmov x10,d11; fmov d12,x13; scvtf d14,x15; fcvtzs x16,d17; br x30.
    let words = [
        0x1e61_2000u32,
        0x1e64_0c62,
        0x1f47_20c5,
        0x1e2e_1009,
        0x9e66_016a,
        0x9e67_01ac,
        0x9e62_01ee,
        0x9e78_0230,
        0xd61f_03c0,
    ];
    let mut mem = HostMem(BTreeMap::new());
    for (offset, word) in words.into_iter().enumerate() {
        for (i, byte) in word.to_le_bytes().into_iter().enumerate() {
            mem.0.insert((offset * 4 + i) as u64, byte);
        }
    }
    let flate = DebugFlate::default();
    let jit = TemplateJit {
        params: vane_arch::template::Params {
            react: &mem,
            trial: &|_| Heat::New,
            flate: &flate,
            root: 0,
            flags: vane_arch::template::Flags::default(),
        },
        pc: 0,
        labels: Default::default(),
        depth: 0,
    };
    let body = alloc::format!("{}", Render(&jit));
    let source = alloc::format!(
        r#"const source={:?};
const mem={{buffer:new ArrayBuffer(64)}};
const dv=new DataView(mem.buffer);
for(const [i,w] of [{words:?}].entries())dv.setUint32(i*4,w,true);
const $={{f:a=>BigInt.asUintN(64,a),r:{{
  "a64:v0":4n===4n?4616189618054758400n:0n,"a64:v1":4616189618054758400n,
  "a64:v3":4619567317775286272n,"a64:v4":4621256167635550208n,
  "a64:v6":4611686018427387904n,"a64:v7":4613937818241073152n,
  "a64:v8":4617315517961601024n,"a64:v11":1234605616436508552n,
  "a64:v17":4631213344936689664n,"a64:x13":9833440827789222417n,
  "a64:x15":18446744073709551604n,"a64:x30":65261n
}},p:{{}},get_page:a=>Number(a),_sys:n=>mem}};
new Function('$','J',source);"#,
        alloc::format!(
            "{}",
            vane_arch::template::CoreJS {
                content: &body,
                flate: &flate,
                flags: vane_arch::template::Flags::default()
            }
        )
    );
    let path = std::env::temp_dir().join(format!("vane-a64-fp-{}.js", std::process::id()));
    std::fs::write(&path, source).unwrap();
    let status = std::process::Command::new("node")
        .arg(&path)
        .status()
        .unwrap();
    let _ = std::fs::remove_file(path);
    assert!(
        status.success(),
        "Node rejected generated AArch64 FP source"
    );
}
