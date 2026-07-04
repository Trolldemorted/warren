#!/usr/bin/env python3
"""Tiny TCP forwarder: 127.0.0.1:<src_port> → 127.0.0.1:<dst_port>.

Used to bridge legacy RABBIT_OBSERVER_URL=http://127.0.0.1:17777 callers
to the new default of 7777, so a claude session that loaded the old env
before the settings.json fix still works without a restart.

Usage: port_forward.py <src_port> <dst_port>
"""
import socket
import sys
import threading


def pipe(src: socket.socket, dst: socket.socket) -> None:
    try:
        while True:
            buf = src.recv(4096)
            if not buf:
                break
            dst.sendall(buf)
    except OSError:
        pass
    finally:
        try:
            src.shutdown(socket.SHUT_RD)
        except OSError:
            pass
        try:
            dst.shutdown(socket.SHUT_WR)
        except OSError:
            pass


def handle(client: socket.socket, dst_addr: tuple[str, int]) -> None:
    try:
        upstream = socket.create_connection(dst_addr, timeout=5)
    except OSError as e:
        print(f"upstream connect failed: {e}", file=sys.stderr)
        client.close()
        return
    threading.Thread(target=pipe, args=(client, upstream), daemon=True).start()
    threading.Thread(target=pipe, args=(upstream, client), daemon=True).start()


def main() -> int:
    if len(sys.argv) != 3:
        print(f"usage: {sys.argv[0]} <src_port> <dst_port>", file=sys.stderr)
        return 2
    src_port = int(sys.argv[1])
    dst_port = int(sys.argv[2])
    dst_addr = ("127.0.0.1", dst_port)

    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", src_port))
    srv.listen(64)
    print(f"forwarding 127.0.0.1:{src_port} -> 127.0.0.1:{dst_port}", flush=True)
    while True:
        client, _ = srv.accept()
        threading.Thread(target=handle, args=(client, dst_addr), daemon=True).start()
    return 0


if __name__ == "__main__":
    sys.exit(main())