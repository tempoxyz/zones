#!/usr/bin/env python3
"""
vsock-to-TCP proxy for Nitro Enclaves.

Forwards connections from the enclave (vsock CID:port) to an external TCP endpoint
(e.g., L1 RPC URL). Runs on the parent EC2 instance.

Nitro Enclaves have no network access. The only way for the enclave to
communicate with the outside world is through vsock — a virtio socket that
connects the enclave to its parent EC2 instance. This proxy listens on a
vsock port and forwards each connection to a TCP target (typically the L1 RPC).

Usage:
    python3 vsock-proxy.py --vsock-port 8000 --target-host rpc.moderato.tempo.xyz --target-port 443
"""

from __future__ import annotations

import argparse
import logging
import selectors
import socket
import ssl
import sys
import threading

# vsock constants
AF_VSOCK = 40  # Address family for vsock
VMADDR_CID_ANY = 0xFFFFFFFF  # Accept connections from any CID
BUFFER_SIZE = 65536

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [vsock-proxy] %(levelname)s %(message)s",
)
log = logging.getLogger(__name__)


def forward(src: socket.socket, dst: socket.socket, label: str) -> None:
    """Forward data from src to dst until either side closes."""
    try:
        while True:
            data = src.recv(BUFFER_SIZE)
            if not data:
                break
            dst.sendall(data)
    except (OSError, BrokenPipeError):
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


def handle_connection(
    vsock_conn: socket.socket,
    target_host: str,
    target_port: int,
    use_tls: bool,
) -> None:
    """Handle a single vsock connection by proxying to the TCP target."""
    try:
        tcp_sock = socket.create_connection((target_host, target_port), timeout=10)
        if use_tls:
            ctx = ssl.create_default_context()
            tcp_sock = ctx.wrap_socket(tcp_sock, server_hostname=target_host)

        log.info("Proxying to %s:%d (tls=%s)", target_host, target_port, use_tls)

        # Bidirectional forwarding
        t1 = threading.Thread(
            target=forward, args=(vsock_conn, tcp_sock, "enclave→target"), daemon=True
        )
        t2 = threading.Thread(
            target=forward, args=(tcp_sock, vsock_conn, "target→enclave"), daemon=True
        )
        t1.start()
        t2.start()
        t1.join()
        t2.join()
    except Exception:
        log.exception("Error proxying connection")
    finally:
        vsock_conn.close()


def main() -> None:
    parser = argparse.ArgumentParser(description="vsock-to-TCP proxy for Nitro Enclaves")
    parser.add_argument("--vsock-port", type=int, required=True, help="vsock port to listen on")
    parser.add_argument("--target-host", type=str, required=True, help="TCP target hostname")
    parser.add_argument("--target-port", type=int, required=True, help="TCP target port")
    parser.add_argument("--no-tls", action="store_true", help="Disable TLS for the target connection")
    args = parser.parse_args()

    use_tls = not args.no_tls and args.target_port == 443

    # Create vsock listener
    vsock = socket.socket(AF_VSOCK, socket.SOCK_STREAM)
    vsock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    vsock.bind((VMADDR_CID_ANY, args.vsock_port))
    vsock.listen(32)

    log.info(
        "Listening on vsock port %d, forwarding to %s:%d (tls=%s)",
        args.vsock_port,
        args.target_host,
        args.target_port,
        use_tls,
    )

    try:
        while True:
            conn, addr = vsock.accept()
            log.info("Accepted connection from CID %s", addr)
            t = threading.Thread(
                target=handle_connection,
                args=(conn, args.target_host, args.target_port, use_tls),
                daemon=True,
            )
            t.start()
    except KeyboardInterrupt:
        log.info("Shutting down.")
    finally:
        vsock.close()


if __name__ == "__main__":
    main()
