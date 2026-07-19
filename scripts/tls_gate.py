#!/usr/bin/env python3
"""创建唯一的本地 TLS gate identity、origin 与 guest trust fixture。"""

from __future__ import annotations

import socket
import ssl
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
GATE_HOSTNAME = "liteos-gate.test"
TLS_ACCEPT_ISOLATION_CLIENTS = 10
TLS_ACCEPT_ISOLATION_DEADLINE_SECONDS = 2.0


def _run(command: list[str]) -> None:
    result = subprocess.run(
        command,
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if result.returncode != 0:
        tail = "\n".join(result.stdout.splitlines()[-40:])
        raise RuntimeError(f"TLS gate command failed: {' '.join(command)}\n{tail}")


def _connect_gate(port: int, timeout: float) -> socket.socket:
    deadline = time.monotonic() + timeout
    while True:
        try:
            return socket.create_connection(("127.0.0.1", port), timeout=timeout)
        except ConnectionRefusedError:
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.01)


def _verify_accept_isolation(port: int, ca_cert: Path) -> None:
    """证明停滞 ClientHello 不会阻塞后续合法 TLS handshake。"""
    context = ssl.create_default_context(cafile=str(ca_cert))
    stalled = _connect_gate(port, TLS_ACCEPT_ISOLATION_DEADLINE_SECONDS)
    started = time.monotonic()
    try:
        for _ in range(TLS_ACCEPT_ISOLATION_CLIENTS):
            remaining = TLS_ACCEPT_ISOLATION_DEADLINE_SECONDS - (time.monotonic() - started)
            if remaining <= 0:
                raise TimeoutError("TLS accept isolation deadline expired")
            with _connect_gate(port, remaining) as connection:
                with context.wrap_socket(
                    connection,
                    server_hostname=GATE_HOSTNAME,
                ):
                    pass
    finally:
        stalled.close()
    elapsed = time.monotonic() - started
    if elapsed > TLS_ACCEPT_ISOLATION_DEADLINE_SECONDS:
        raise RuntimeError(
            f"TLS accept isolation exceeded {TLS_ACCEPT_ISOLATION_DEADLINE_SECONDS}s: "
            f"{elapsed:.3f}s"
        )


def start_https_gate(
    directory: Path,
    root_directory: Path = ROOT,
    ports: range = range(18443, 18544),
) -> tuple[subprocess.Popen[bytes], int, Path]:
    """创建临时 CA/server identity，并启动只供 QEMU gate 消费的 HTTPS origin。

    Args:
        directory: 调用方独占的 identity 与证书目录。
        root_directory: HTTPS origin 的 document root。
        ports: 当前 gate 独占的 host port domain。

    Returns:
        已监听的 server process、host port 与 CA certificate。

    Raises:
        RuntimeError: 证书生成失败或 port domain 中没有可用端口。
    """
    ca_key = directory / "ca.key"
    ca_cert = directory / "ca.pem"
    server_key = directory / "server.key"
    server_csr = directory / "server.csr"
    server_cert = directory / "server.pem"
    extensions = directory / "server.ext"
    extensions.write_text(
        "basicConstraints=CA:FALSE\n"
        "keyUsage=digitalSignature,keyEncipherment\n"
        "extendedKeyUsage=serverAuth\n"
        f"subjectAltName=DNS:{GATE_HOSTNAME}\n"
    )
    _run(
        [
            "openssl",
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-sha256",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=LiteOS Gate CA",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
            "-addext",
            "keyUsage=critical,keyCertSign,cRLSign",
            "-keyout",
            str(ca_key),
            "-out",
            str(ca_cert),
        ]
    )
    _run(
        [
            "openssl",
            "req",
            "-newkey",
            "rsa:2048",
            "-sha256",
            "-nodes",
            "-subj",
            f"/CN={GATE_HOSTNAME}",
            "-keyout",
            str(server_key),
            "-out",
            str(server_csr),
        ]
    )
    _run(
        [
            "openssl",
            "x509",
            "-req",
            "-in",
            str(server_csr),
            "-CA",
            str(ca_cert),
            "-CAkey",
            str(ca_key),
            "-CAcreateserial",
            "-days",
            "1",
            "-sha256",
            "-extfile",
            str(extensions),
            "-out",
            str(server_cert),
        ]
    )
    for port in ports:
        server = subprocess.Popen(
            [
                sys.executable,
                str(ROOT / "scripts/https_gate.py"),
                "--cert",
                str(server_cert),
                "--key",
                str(server_key),
                "--port",
                str(port),
                "--root",
                str(root_directory),
            ],
            cwd=ROOT,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.STDOUT,
        )
        try:
            server.wait(timeout=0.05)
        except subprocess.TimeoutExpired:
            try:
                _verify_accept_isolation(port, ca_cert)
            except (OSError, TimeoutError, ssl.SSLError) as error:
                server.terminate()
                server.wait(timeout=3)
                raise RuntimeError(f"TLS accept isolation failed: {error}") from error
            return server, port, ca_cert
    raise RuntimeError(f"no free HTTPS gate port in {ports.start}..{ports.stop - 1}")


def install_runtime_tls_identity(
    image: Path,
    gate_ca: Path,
    directory: Path,
    debugfs: Path,
) -> None:
    """向 disposable image 的 package-owned CA target 追加 gate CA，并保留标准 symlink。"""
    public_bundle = directory / "public-cert.pem"
    _run(
        [
            str(debugfs),
            "-R",
            f"dump /etc/ssl/certs/ca-certificates.crt {public_bundle}",
            str(image),
        ]
    )
    bundle = directory / "gate-cert.pem"
    bundle.write_bytes(public_bundle.read_bytes() + b"\n" + gate_ca.read_bytes())
    hosts = directory / "hosts"
    hosts.write_text(f"10.0.2.2 {GATE_HOSTNAME}\n")
    commands = directory / "tls.debugfs"
    commands.write_text(
        "rm /etc/ssl/certs/ca-certificates.crt\n"
        f"write {bundle} /etc/ssl/certs/ca-certificates.crt\n"
        f"write {hosts} /etc/hosts\n"
    )
    _run([str(debugfs), "-w", "-f", str(commands), str(image)])
