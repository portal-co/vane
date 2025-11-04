use crate::*;
macro_rules! renders {
    ($($a:ident),*) => {
        $(paste::paste!{
            pub trait [<$a Display>]{
                fn $a(&self, f: &mut Formatter) -> core::fmt::Result;
            }
            pub trait [<$a WasmJit>]{
                fn $a<'a>(&'a self) -> Box<dyn Iterator<Item = $crate::JitOpcode<'a>> + 'a>;
            }
            #[derive(Clone, Copy)]
            pub struct $a<'a,T: ?Sized = dyn [<$a Display>] + 'a>(pub &'a T);
            const _: () = {
                impl<T: [<$a Display>] + ?Sized> Display for $a<'_,T>{
                    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
                        self.0.$a(f)
                    }
                }
                impl<'a,T: [<$a Display>] + ?Sized> [<$a Display>] for &'a T{
                    fn $a(&self, f: &mut Formatter) -> core::fmt::Result{
                        (&**self).$a(f)
                    }
                }
                impl<'b,T: [<$a WasmJit>] + ?Sized> [<$a WasmJit>] for &'b T{
                    fn $a<'a>(&'a self) -> Box<dyn Iterator<Item = $crate::JitOpcode<'a>> + 'a>{
                        (&**self).$a()
                    }
                }
                impl<T: [<$a WasmJit>] + ?Sized> WasmJit for $a<'_,T>{
                    fn jit<'a>(&'a self) -> Box<dyn Iterator<Item = JitOpcode<'a>> + 'a>{
                        self.0.$a()
                    }
                }
            };
        })*
    };
}
renders!(Riscv);
