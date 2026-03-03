#![no_std]
extern crate alloc;

pub mod flate;

pub use flate::{DebugFlate, Flate, ReleaseFlate};
