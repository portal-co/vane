use core::fmt::Display;

use alloc::sync::Arc;

pub trait Flate {
    fn flate<'a>(&'a self, a: &'a str) -> Arc<dyn Display + 'a>;
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DebugFlate {}
impl Flate for DebugFlate {
    fn flate<'a>(&'a self, a: &'a str) -> Arc<dyn Display + 'a> {
        return Arc::new(a);
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReleaseFlate {}
impl Flate for ReleaseFlate {
    fn flate<'a>(&'a self, a: &'a str) -> Arc<dyn Display + 'a> {
        match a {
            "max64" => Arc::new("f"),
            "max32" => Arc::new("g"),
            "signed" => Arc::new("s"),
            "unsigned" => Arc::new("u"),
            "data" => Arc::new("d"),
            a => Arc::new(a),
        }
    }
}
