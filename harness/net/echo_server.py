#!/usr/bin/env python3
"""Host echo server for the net gate (PLAN_PI_IN_VM P1).

Sends a banner on connect, then echoes bytes back. Stdlib only.

Usage: echo_server.py [port]   (default 17777, bound to 127.0.0.1)
Prints the chosen port as `PORT <n>` on stdout.
"""
import socket
import sys
import threading

BANNER = b"VANE-ECHO-BANNER\n"


def handle(conn: socket.socket) -> None:
    with conn:
        conn.sendall(BANNER)
        while True:
            data = conn.recv(4096)
            if not data:
                return
            conn.sendall(data)


def main() -> None:
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 17777
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", port))
    srv.listen(8)
    print(f"PORT {srv.getsockname()[1]}", flush=True)
    while True:
        conn, _ = srv.accept()
        threading.Thread(target=handle, args=(conn,), daemon=True).start()


if __name__ == "__main__":
    main()
