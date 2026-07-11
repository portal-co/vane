//! Minimal end-to-end demo of Mode B (server-hosted, browser-executed
//! blocks): serves a WebSocket endpoint a wsdom-connected browser tab can
//! attach to, then drives a tiny hand-assembled RISC-V program
//! (`auipc x1,0; jalr x0,x1,0` — an infinite tight loop entirely through
//! `TailCall`) for a handful of iterations to exercise the full
//! compile -> ship -> await-exit -> recompile round trip.
//!
//! Run with `cargo run -p vane-wsdom --example serve`, then open
//! `crates/vane-wsdom/examples/index.html` in a browser (or serve it) and
//! watch the WebSocket frames in devtools.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::{Router, extract::ws::WebSocketUpgrade, response::Response, routing::get};
use vane_riscv::{Mem, template::Flags};

const ENTRY_PC: u64 = 0x1000;
/// Stop after this many blocks so the demo terminates on its own.
const MAX_BLOCKS: u32 = 5;

#[tokio::main]
async fn main() {
    let router = Router::new().route("/ws", get(handler));
    let listener = tokio::net::TcpListener::bind("0.0.0.0:4000")
        .await
        .expect("bind 0.0.0.0:4000");
    println!("listening on ws://0.0.0.0:4000/ws");
    println!("open crates/vane-wsdom/examples/index.html in a browser to connect");
    axum::serve(listener, router).await.expect("serve");
}

async fn handler(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| async move {
        println!("connection opened");
        let mem = demo_mem();
        let blocks = Arc::new(AtomicU32::new(0));
        let counter = blocks.clone();
        let outcome = vane_wsdom::serve_connection(
            socket,
            mem,
            ENTRY_PC,
            Flags::default(),
            vane_wsdom::DEFAULT_MEM_SIZE,
            move |_next_pc| counter.fetch_add(1, Ordering::Relaxed) >= MAX_BLOCKS,
        )
        .await;
        match outcome {
            wsdom_axum::Output::Done(Ok(final_pc)) => {
                println!(
                    "connection finished after {} block(s), final pc=0x{final_pc:x}",
                    blocks.load(Ordering::Relaxed)
                );
            }
            wsdom_axum::Output::Done(Err(e)) => {
                println!("connection finished with error after {} block(s): {e}", blocks.load(Ordering::Relaxed));
            }
            wsdom_axum::Output::ConnectionClosed => println!("connection closed by peer"),
            wsdom_axum::Output::AxumError(e) => println!("axum error: {e}"),
            wsdom_axum::Output::WsdomError(e) => println!("wsdom error: {e:?}"),
        }
    })
}

/// `auipc x1, 0` followed by `jalr x0, x1, 0` at `ENTRY_PC`: loads the
/// current PC into `x1`, then jumps to it — a single-block infinite loop
/// that always exits its trace via `StackOp::TailCall` (never a trap or
/// ecall), so it exercises exactly the exit path `run_block` implements.
fn demo_mem() -> Mem {
    let mut mem = Mem::default();
    let auipc_x1_0: u32 = 0x00000097;
    let jalr_x0_x1_0: u32 = 0x00008067;
    for (i, b) in auipc_x1_0.to_le_bytes().into_iter().enumerate() {
        mem.write_byte(ENTRY_PC + i as u64, b);
    }
    for (i, b) in jalr_x0_x1_0.to_le_bytes().into_iter().enumerate() {
        mem.write_byte(ENTRY_PC + 4 + i as u64, b);
    }
    mem
}
