# Networking harness + runtime socket support matrix (PLAN_PI_IN_VM §4, R3)

The tunnel's host side speaks through `vane_host::SocketBackend`. Bindings:

| Binding | Target | Runtime | Mechanism | Status |
| --- | --- | --- | --- | --- |
| `StdSockets` | native (macOS/Linux) | — | `std::net::TcpStream` | tested (echo test, gates) |
| `StdSockets` | `wasm32-wasmer-wasi` | wasmer (WASIX) | std lowers to WASIX `sock_*` | authored, runtime-untested |
| `WasixSockets` (`wasix-raw` feature) | `wasm32-wasip1` | runtimes carrying the unofficial socket extension / WASIX `sock_*` | raw `wasix_32v1` imports | compile-checked only — validate on first wasmer/wazero-sock run (R3) |

ABI notes for `WasixSockets` live in `crates/vane-net/src/wasix_sockets.rs`
(port is native-endian in memory per wasmer's `read_ip_port`; port byte
order differs on wasmer's own write path — a known upstream inconsistency).

Preview1 stock sockets (`sock_accept`/`sock_recv`/`sock_send`/
`sock_shutdown` only — no connect/listen) are NOT sufficient for the
tunnel; preview2 `wasi:sockets` is out of scope for this plan.

## Files

- `echo_server.py` — host echo server (banner on connect + echo).
- `guest-init.sh` — in-guest init for the echo gate.
- `../run-net-echo-gate.sh` — P1 gate.
