use crate::{template::TemplateJit, *};
#[macro_export]
macro_rules! renders {
    ($($a:ident),*) => {
        $($crate::paste::paste!{
            pub trait [<$a Display>]{
                fn $a(&self, f: &mut $crate::core::fmt::Formatter) -> $crate::core::fmt::Result;
            }
            pub trait [<$a WasmJit>]{
                fn $a<'a>(&'a self, ctx: &'a (dyn $crate::WasmJitCtx + 'a)) -> $crate::alloc::boxed::Box<dyn $crate::core::iter::Iterator<Item = $crate::JitOpcode<'a>> + 'a>;
            }
            pub struct [<Template $a>];
            #[derive(Clone, Copy)]
            pub struct $a<'a,T: ?Sized = dyn [<$a Display>] + 'a>(pub &'a T);
            const _: () = {
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
                impl<'b,T: [<$a WasmJit>] + ?Sized> [<$a WasmJit>] for &'b T{
                    fn $a<'a>(&'a self, ctx: &'a (dyn $crate::WasmJitCtx + 'a)) -> $crate::alloc::boxed::Box<dyn $crate::core::iter::Iterator<Item = $crate::JitOpcode<'a>> + 'a>{
                        (&**self).$a(ctx)
                    }
                }
                impl<T: [<$a WasmJit>] + ?Sized> $crate::WasmJit for $a<'_,T>{
                    fn jit<'a>(&'a self, ctx: &'a (dyn $crate::WasmJitCtx + 'a)) -> $crate::alloc::boxed::Box<dyn $crate::core::iter::Iterator<Item = $crate::JitOpcode<'a>> + 'a>{
                        self.0.$a(ctx)
                    }
                }
                impl $crate::template::TemplateJS for [<Template $a>]{
                    type Ty<'a> = $a<'a>;
                    type Wasm<'a> = $a<'a,dyn [<$a WasmJit>] + 'a>;

                    fn template_jit_js<'a>(&self, j: &'a $crate::template::TemplateJit<'_>) -> Self::Ty<'a> {
                        $a(j)
                    }

                    fn template_jit_wasm<'a>(&self, j: &'a $crate::template::TemplateJit<'_>) -> Self::Wasm<'a> {
                        $a(j)
                    }
                }
            };
        })*
    };
}
renders!(Riscv);
