#![no_std]
extern crate alloc;

pub mod flate;
pub mod mem;

pub use flate::{DebugFlate, Flate, ReleaseFlate};
