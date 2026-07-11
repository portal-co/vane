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
        let mem = demo_mem();
        let mut blocks = 0u32;
        vane_wsdom::serve_connection(
            socket,
            mem,
            ENTRY_PC,
            Flags::default(),
            vane_wsdom::DEFAULT_MEM_SIZE,
            move |_next_pc| {
                blocks += 1;
                blocks > MAX_BLOCKS
            },
        )
        .await;
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
