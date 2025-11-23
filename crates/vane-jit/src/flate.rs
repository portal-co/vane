use core::fmt::Display;

use alloc::boxed::Box;

pub trait Flate {
    fn flate<'a>(&'a self, a: &'a str) -> Box<dyn Display + 'a>;
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DebugFlate {}
impl Flate for DebugFlate {
    fn flate<'a>(&'a self, a: &'a str) -> Box<dyn Display + 'a> {
        return Box::new(a);
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReleaseFlate {}
impl Flate for ReleaseFlate {
    fn flate<'a>(&'a self, a: &'a str) -> Box<dyn Display + 'a> {
        match a{
            "max64" => Box::new("f"),
            "max32" => Box::new("g"),
            "signed" => Box::new("s"),
            "unsigned" => Box::new("u"),
            "data" => Box::new("d"),
            a => Box::new(a)
        }
    }
}
