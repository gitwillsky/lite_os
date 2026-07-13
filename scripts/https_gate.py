#!/usr/bin/env python3
"""为 QEMU slirp gate 提供只监听 host loopback 的固定 HTTPS origin。"""

from __future__ import annotations

import argparse
import http.server
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


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cert", type=Path, required=True)
    parser.add_argument("--key", type=Path, required=True)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--root", type=Path, default=Path.cwd())
    args = parser.parse_args()
    handler = partial(GateRequestHandler, directory=str(args.root))
    server = http.server.ThreadingHTTPServer(("127.0.0.1", args.port), handler)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(args.cert, args.key)
    server.socket = context.wrap_socket(server.socket, server_side=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
