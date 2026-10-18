//! Pumped tunnel (PLAN_PI_IN_VM M3): a background thread owns the smoltcp
//! clock and services flows on a real-time interval, while the device-facing
//! [`NetBackend`] impl only moves frames across the lock.
//!
//! Threading: `std::thread`, which is a real thread natively and a WASIX
//! thread on `wasm32-wasmer-wasi` — so this one implementation is the M3
//! "WASIX-threaded I/O pump" on that target. (The wasi-threads variant for
//! other runtimes is M12.)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use vane_host::SocketBackend;
use vane_riscv::virtio_net::NetBackend;

use crate::Tunnel;

/// A [`NetBackend`] wrapping a [`Tunnel`] whose clock and flow servicing run
/// on a background thread.
pub struct PumpedTunnel<B: SocketBackend + Send + 'static> {
    inner: Arc<Mutex<Tunnel<B>>>,
    stop: Arc<AtomicBool>,
    pump: Option<JoinHandle<()>>,
}

impl<B: SocketBackend + Send + 'static> PumpedTunnel<B> {
    /// Wrap `tunnel`, spawning the pump thread with `interval` between
    /// service passes (also the clock quantum).
    pub fn new(tunnel: Tunnel<B>, interval: Duration) -> Self {
        let inner = Arc::new(Mutex::new(tunnel));
        let stop = Arc::new(AtomicBool::new(false));
        let pump = {
            let inner = Arc::clone(&inner);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(interval);
                    if let Ok(mut t) = inner.lock() {
                        t.poll(interval.as_millis() as u32);
                    }
                }
            })
        };
        PumpedTunnel {
            inner,
            stop,
            pump: Some(pump),
        }
    }
}

impl<B: SocketBackend + Send + 'static> NetBackend for PumpedTunnel<B> {
    fn send_frame(&mut self, frame: &[u8]) {
        if let Ok(mut t) = self.inner.lock() {
            t.push_guest_frame(frame);
        }
    }

    fn poll_recv(&mut self) -> Option<alloc::vec::Vec<u8>> {
        self.inner.lock().ok()?.pop_host_frame()
    }
}

impl<B: SocketBackend + Send + 'static> Drop for PumpedTunnel<B> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.pump.take() {
            h.join().ok();
        }
    }
}
