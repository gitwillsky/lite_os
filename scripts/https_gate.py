#!/usr/bin/env python3
"""为 QEMU slirp gate 提供只监听 host loopback 的固定 HTTPS origin。"""

from __future__ import annotations

import argparse
import http.server
import socket
import ssl
import time
from functools import partial
from pathlib import Path


class GateRequestHandler(http.server.SimpleHTTPRequestHandler):
    """提供静态内容、标准 redirect 与可用于 deadline 验证的慢响应。"""

    def do_GET(self) -> None:
        if self.path == "/redirect":
            self.send_response(302)
            self.send_header("Location", "/payload.bin")
            self.end_headers()
            return
        if self.path == "/slow":
            time.sleep(5)
            self.path = "/payload.bin"
        super().do_GET()

    def log_message(self, format: str, *args: object) -> None:
        pass


class ThreadedTlsHttpServer(http.server.ThreadingHTTPServer):
    """在 per-connection worker 内完成 TLS 握手，隔离迟滞或无效客户端。"""

    daemon_threads = True
    handshake_timeout_seconds = 5.0

    def __init__(
        self,
        server_address: tuple[str, int],
        request_handler: type[http.server.BaseHTTPRequestHandler],
        context: ssl.SSLContext,
    ) -> None:
        self.tls_context = context
        super().__init__(server_address, request_handler)

    def process_request_thread(
        self,
        request: socket.socket,
        client_address: tuple[str, int],
    ) -> None:
        connection: ssl.SSLSocket | socket.socket = request
        try:
            # TLS handshake 必须在线程池已经接管 accepted socket 后执行；若包装 listening
            # socket，SSLSocket.accept 会在唯一 accept owner 内同步握手，单个半连接即可
            # head-of-line block 全部后续 gate 请求。
            request.settimeout(self.handshake_timeout_seconds)
            connection = self.tls_context.wrap_socket(request, server_side=True)
            connection.settimeout(None)
        except (OSError, TimeoutError, ssl.SSLError):
            self.shutdown_request(connection)
            return
        try:
            self.finish_request(connection, client_address)
        except Exception:
            self.handle_error(connection, client_address)
        finally:
            self.shutdown_request(connection)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cert", type=Path, required=True)
    parser.add_argument("--key", type=Path, required=True)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--root", type=Path, default=Path.cwd())
    args = parser.parse_args()
    handler = partial(GateRequestHandler, directory=str(args.root))
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(args.cert, args.key)
    server = ThreadedTlsHttpServer(("127.0.0.1", args.port), handler, context)
    server.serve_forever()


if __name__ == "__main__":
    main()
