//! Browser execution smoke test for the public AArch64 JS lowering path.
//! It deliberately avoids the RV64 WASM-bindgen reactor: the generated
//! `CoreJS` source is evaluated with a browser-provided `$` state object.

use super::*;
use alloc::{collections::BTreeMap, format};
use core::fmt::{self, Formatter};
use js_sys::{BigInt, Function, Object, Reflect, Uint8Array, global};
use vane_arch::DebugFlate;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

struct Mem(BTreeMap<u64, u8>);
impl vane_arch::JitCtx for Mem {
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

fn set(object: &Object, key: &str, value: &JsValue) {
    Reflect::set(object, &JsValue::from_str(key), value).unwrap();
}

#[wasm_bindgen_test]
async fn aarch64_integer_microcorpus_executes_in_browser() {
    // movz x0,#10; br x30. This is the minimal checked-in A64 browser corpus
    // sentinel; richer raw-word execution is covered by the native corpus.
    let words = [0xd280_0140u32, 0xd61f_03c0];
    let mut mem = Mem(BTreeMap::new());
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
    let body = format!("{}", Render(&jit));
    let source = format!(
        "{}",
        vane_arch::template::CoreJS {
            content: &body,
            flate: &flate,
            flags: vane_arch::template::Flags::default(),
        }
    );
    let memory = Uint8Array::new_with_length(64);
    for (i, word) in words.into_iter().enumerate() {
        for (j, byte) in word.to_le_bytes().into_iter().enumerate() {
            memory.set_index((i * 4 + j) as u32, byte);
        }
    }
    let state = Object::new();
    set(&state, "a64:x30", &BigInt::from(0xfeedu32).into());
    let runtime = Object::new();
    set(&runtime, "r", &state);
    set(&runtime, "p", &Object::new().into());
    // The generated source only invokes `_sys`/`get_page` for CheckCode.
    set(
        &runtime,
        "get_page",
        &Function::new_with_args("a", "return Number(a);").into(),
    );
    set(&global().into(), "__vane_a64_mem", &memory.into());
    set(
        &runtime,
        "_sys",
        &Function::new_no_args("return globalThis.__vane_a64_mem;").into(),
    );
    set(
        &runtime,
        "f",
        &Function::new_with_args("x", "return BigInt.asUintN(64,x);").into(),
    );
    let factory = Function::new_with_args("$,J", &source);
    let exit = Function::new_with_args("x", "globalThis.__vane_a64_exit=x;");
    let compiled = factory.call2(&JsValue::NULL, &runtime, &exit).unwrap();
    let run = Function::from(compiled);
    JsFuture::from(js_sys::Promise::from(run.call0(&JsValue::NULL).unwrap()))
        .await
        .unwrap();
    assert_eq!(
        Function::new_no_args(
            "return new DataView(globalThis.__vane_a64_mem.buffer).getUint32(0,true);",
        )
        .call0(&JsValue::NULL)
        .unwrap()
        .as_f64(),
        Some(words[0] as f64)
    );
    let exit_value = Reflect::get(&global().into(), &JsValue::from_str("__vane_a64_exit")).unwrap();
    assert!(
        exit_value.is_undefined(),
        "generated trace took its CheckCode miss path: {}",
        Function::new_with_args("x", "return String(x);")
            .call1(&JsValue::NULL, &exit_value)
            .unwrap()
            .as_string()
            .unwrap()
    );
    let observed_x0 = Reflect::get(&state, &JsValue::from_str("a64:x0")).unwrap();
    assert!(
        Function::new_with_args("x", "return x===10n;")
            .call1(&JsValue::NULL, &observed_x0)
            .unwrap()
            .as_bool()
            .unwrap(),
        "x0 was {}",
        Function::new_with_args("x", "return String(x);")
            .call1(&JsValue::NULL, &observed_x0)
            .unwrap()
            .as_string()
            .unwrap()
    );
    // The trace reaches its tail-call exit after writing X0; JS `J` is a
    // dispatcher callback and is intentionally not invoked by this direct
    // corpus harness.
}
