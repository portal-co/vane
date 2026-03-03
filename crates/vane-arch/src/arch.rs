
#[macro_export]
macro_rules! renders {
    ($($a:ident),*) => {
        $($crate::paste::paste!{
            pub trait [<$a Display>]{
                fn $a(&self, f: &mut $crate::core::fmt::Formatter) -> $crate::core::fmt::Result;
            }
            #[cfg(feature = "target-wasm")]
            pub trait [<$a WasmJit>]{
                fn $a<'a>(&'a self, ctx: &'a (dyn $crate::WasmJitCtx + 'a)) -> impl $crate::core::iter::Iterator<Item = $crate::JitOpcode<'a>> + 'a;
            }
            #[cfg(feature = "target-wasm")]
            pub trait [<$a WasmJitDyn>]{
                fn $a<'a>(&'a self, ctx: &'a (dyn $crate::WasmJitCtx + 'a)) -> $crate::alloc::boxed::Box<dyn $crate::core::iter::Iterator<Item = $crate::JitOpcode<'a>> + 'a>;
            }
            pub struct [<Template $a>];
            #[derive(Clone, Copy)]
            pub struct $a<'a,T: ?Sized = dyn [<$a Display>] + 'a>(pub &'a T);
            const _: () = {
                #[cfg(feature = "target-wasm")]
                impl<T: [<$a WasmJit>] + ?Sized> [<$a WasmJitDyn>] for T{
                    fn $a<'a>(&'a self, ctx: &'a (dyn $crate::WasmJitCtx + 'a)) -> $crate::alloc::boxed::Box<dyn $crate::core::iter::Iterator<Item = $crate::JitOpcode<'a>> + 'a>{
                        $crate::alloc::boxed::Box::new(self.$a(ctx))
                    }
                }
                #[cfg(feature = "target-wasm")]
                impl<'b> [<$a WasmJit>] for dyn [<$a WasmJitDyn>] + 'b{
                    fn $a<'a>(&'a self, ctx: &'a (dyn $crate::WasmJitCtx + 'a)) -> impl $crate::core::iter::Iterator<Item = $crate::JitOpcode<'a>> + 'a{
                        self.$a(ctx)
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
                #[cfg(feature = "target-wasm")]
                impl<'b,T: [<$a WasmJit>] + ?Sized> [<$a WasmJit>] for &'b T{
                    fn $a<'a>(&'a self, ctx: &'a (dyn $crate::WasmJitCtx + 'a)) -> impl $crate::core::iter::Iterator<Item = $crate::JitOpcode<'a>> + 'a{
                        (&**self).$a(ctx)
                    }
                }
                #[cfg(feature = "target-wasm")]
                impl<T: [<$a WasmJit>] + ?Sized> $crate::WasmJit for $a<'_,T>{
                    fn jit<'a>(&'a self, ctx: &'a (dyn $crate::WasmJitCtx + 'a)) -> impl $crate::core::iter::Iterator<Item = $crate::JitOpcode<'a>> + 'a{
                        self.0.$a(ctx)
                    }
                }
                impl $crate::template::ArchTarget for [<Template $a>]{
                    #[cfg(feature = "target-js")]
                    type Js<'a> = $a<'a>;
                    #[cfg(feature = "target-wasm")]
                    type Wasm<'a> = $a<'a, dyn [<$a WasmJitDyn>] + 'a>;

                    #[cfg(feature = "target-js")]
                    fn target_js<'a>(&self, j: &'a $crate::template::TemplateJit<'_>) -> Self::Js<'a> {
                        $a(j)
                    }
                    #[cfg(feature = "target-wasm")]
                    fn target_wasm<'a>(&self, j: &'a $crate::template::TemplateJit<'_>) -> Self::Wasm<'a> {
                        $a(j)
                    }
                }
            };
        })*
    };
}
