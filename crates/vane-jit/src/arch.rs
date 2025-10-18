use crate::*;
macro_rules! renders {
    ($($a:ident),*) => {
        $(paste::paste!{
            pub trait [<$a Display>]{
                fn $a(&self, f: &mut Formatter) -> core::fmt::Result;
            }
            #[derive(Clone, Copy)]
            pub struct $a<'a>(pub &'a (dyn [<$a Display>] + 'a));
            impl Display for $a<'_>{
                fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
                    self.0.$a(f)
                }
            }
        })*
    };
}
renders!(Riscv);
