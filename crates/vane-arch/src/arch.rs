// ---- feature-gate helper macros ----------------------------------------
//
// These macros are defined *here* in vane-arch, so the `cfg` check is
// evaluated against **vane-arch's** feature flags rather than the caller's.
// That means downstream crates can use `$crate::cfg_target_js! { … }` /
// `$crate::cfg_target_wasm! { … }` without re-declaring the feature names
// themselves.
//
// Each pair: one variant is compiled in when the feature is enabled, the
// other when it is disabled.  Exactly one survives into the rlib, and that
// is the one callers see.

/// Expands to `$item` only when the `target-js` feature is enabled in
/// `vane-arch`.  Use this instead of `#[cfg(feature = "target-js")]` inside
/// macros so the check is always anchored to `vane-arch`'s own feature set.
#[cfg(feature = "target-js")]
#[macro_export]
macro_rules! cfg_target_js {
    ($($t:tt)*) => { $($t)* };
}

/// No-op variant compiled in when `target-js` is disabled.
#[cfg(not(feature = "target-js"))]
#[macro_export]
macro_rules! cfg_target_js {
    ($($t:tt)*) => {};
}

/// Expands to `$item` only when the `target-wasm` feature is enabled in
/// `vane-arch`.  Use this instead of `#[cfg(feature = "target-wasm")]` inside
/// macros so the check is always anchored to `vane-arch`'s own feature set.
#[cfg(feature = "target-wasm")]
#[macro_export]
macro_rules! cfg_target_wasm {
    ($($t:tt)*) => { $($t)* };
}

/// No-op variant compiled in when `target-wasm` is disabled.
#[cfg(not(feature = "target-wasm"))]
#[macro_export]
macro_rules! cfg_target_wasm {
    ($($t:tt)*) => {};
}

// ---- ArchTarget impl helper ---------------------------------------------
//
// `renders!` calls this at its tail, but it is also exported so that
// architectures which need a hand-written `ArchTarget` impl (rather than the
// generated one) can use it directly without duplicating the cfg machinery.

/// Implement [`crate::template::ArchTarget`] for a `Template*` marker struct.
///
/// # Parameters
/// - `$Template` — the marker struct produced by `renders!` (e.g. `TemplateRiscv`)
/// - `$Wrapper` — the newtype wrapper that implements both `Display` (via
///   `*Display`) and `WasmJit` (via `*WasmJit`), e.g. `Riscv`
/// - `$WasmDyn` — the object-safe dyn-dispatch version of the wasm trait,
///   e.g. `RiscvWasmJitDyn`
///
/// The macro uses [`cfg_target_js!`] / [`cfg_target_wasm!`] internally, so
/// the feature checks are always evaluated against `vane-arch`'s feature set.
#[macro_export]
macro_rules! impl_arch_target {
    ($Template:ident, $Wrapper:ident, $WasmDyn:ident) => {
        impl $crate::template::ArchTarget for $Template {
            $crate::cfg_target_js! {
                type Js<'a> = $Wrapper<'a>;
                fn target_js<'a>(
                    &self,
                    j: &'a $crate::template::TemplateJit<'_>,
                ) -> Self::Js<'a> {
                    $Wrapper(j)
                }
            }
            $crate::cfg_target_wasm! {
                type Wasm<'a> = $Wrapper<'a, dyn $WasmDyn + 'a>;
                fn target_wasm<'a>(
                    &self,
                    j: &'a $crate::template::TemplateJit<'_>,
                ) -> Self::Wasm<'a> {
                    $Wrapper(j)
                }
            }
        }
    };
}

#[macro_export]
macro_rules! renders {
    ($($a:ident),*) => {
        $($crate::paste::paste!{
            pub trait [<$a Display>]{
                fn $a(&self, f: &mut $crate::core::fmt::Formatter) -> $crate::core::fmt::Result;
            }
            $crate::cfg_target_wasm! {
                pub trait [<$a WasmJit>]{
                    fn $a<'a>(&'a self, ctx: &'a (dyn $crate::WasmJitCtx + 'a)) -> impl $crate::core::iter::Iterator<Item = $crate::JitOpcode<'a>> + 'a;
                }
                pub trait [<$a WasmJitDyn>]{
                    fn $a<'a>(&'a self, ctx: &'a (dyn $crate::WasmJitCtx + 'a)) -> $crate::alloc::boxed::Box<dyn $crate::core::iter::Iterator<Item = $crate::JitOpcode<'a>> + 'a>;
                }
            }
            pub struct [<Template $a>];
            #[derive(Clone, Copy)]
            pub struct $a<'a,T: ?Sized = dyn [<$a Display>] + 'a>(pub &'a T);
            const _: () = {
                $crate::cfg_target_wasm! {
                    impl<T: [<$a WasmJit>] + ?Sized> [<$a WasmJitDyn>] for T{
                        fn $a<'a>(&'a self, ctx: &'a (dyn $crate::WasmJitCtx + 'a)) -> $crate::alloc::boxed::Box<dyn $crate::core::iter::Iterator<Item = $crate::JitOpcode<'a>> + 'a>{
                            $crate::alloc::boxed::Box::new(self.$a(ctx))
                        }
                    }
                    impl<'b> [<$a WasmJit>] for dyn [<$a WasmJitDyn>] + 'b{
                        fn $a<'a>(&'a self, ctx: &'a (dyn $crate::WasmJitCtx + 'a)) -> impl $crate::core::iter::Iterator<Item = $crate::JitOpcode<'a>> + 'a{
                            self.$a(ctx)
                        }
                    }
                    impl<'b,T: [<$a WasmJit>] + ?Sized> [<$a WasmJit>] for &'b T{
                        fn $a<'a>(&'a self, ctx: &'a (dyn $crate::WasmJitCtx + 'a)) -> impl $crate::core::iter::Iterator<Item = $crate::JitOpcode<'a>> + 'a{
                            (&**self).$a(ctx)
                        }
                    }
                    impl<T: [<$a WasmJit>] + ?Sized> $crate::WasmJit for $a<'_,T>{
                        fn jit<'a>(&'a self, ctx: &'a (dyn $crate::WasmJitCtx + 'a)) -> impl $crate::core::iter::Iterator<Item = $crate::JitOpcode<'a>> + 'a{
                            self.0.$a(ctx)
                        }
                    }
                }
                impl<T: [<$a Display>] + ?Sized> $crate::core::fmt::Display for $a<'_,T>{
                    fn fmt(&self, f: &mut $crate::core::fmt::Formatter) -> $crate::core::fmt::Result {
                        self.0.$a(f)
                    }
                }
                impl<'a,T: [<$a Display>] + ?Sized> [<$a Display>] for &'a T{
                    fn $a(&self, f: &mut $crate::core::fmt::Formatter) -> $crate::core::fmt::Result{
                        (&**self).$a(f)
                    }
                }
                $crate::impl_arch_target!([<Template $a>], $a, [<$a WasmJitDyn>]);
            };
        })*
    };
}
