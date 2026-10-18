//! The GuestCoroutine driver: local futures run to completion in one resume;
//! a future that pends (a non-local host round-trip) reports Parked and
//! completes after resume (once the round-trip wakes the waker).
#![cfg(feature = "async-host")]

use std::cell::Cell;
use std::rc::Rc;
use std::task::{Context, Poll};
use vane_arch::interp::{CoroutineState, GuestCoroutine};

#[test]
fn local_future_completes_in_one_resume() {
    let mut c = GuestCoroutine::new(async { 42u64 });
    match c.resume() {
        CoroutineState::Done(v) => assert_eq!(v, 42),
        CoroutineState::Parked => panic!("a ready future must not park"),
    }
}

/// A future that pends until an external flag is set, then wakes.
struct Gated {
    ready: Rc<Cell<bool>>,
    value: u64,
}
impl std::future::Future for Gated {
    type Output = u64;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u64> {
        if self.ready.get() {
            Poll::Ready(self.value)
        } else {
            // Register interest: when the host round-trip completes it calls
            // wake(), which sets the coroutine's woken flag.
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

#[test]
fn pending_future_parks_then_completes_after_wake() {
    let ready = Rc::new(Cell::new(false));
    let mut c = GuestCoroutine::new(Gated {
        ready: ready.clone(),
        value: 7,
    });
    // First resume: the future pends → Parked, and it registered a wake.
    assert!(matches!(c.resume(), CoroutineState::Parked));
    assert!(
        c.is_woken(),
        "the pending host op should have registered a wake"
    );
    // Complete the "round-trip" and resume.
    ready.set(true);
    match c.resume() {
        CoroutineState::Done(v) => assert_eq!(v, 7),
        CoroutineState::Parked => panic!("should complete once the gate opened"),
    }
}
