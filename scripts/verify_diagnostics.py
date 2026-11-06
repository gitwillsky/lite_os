#!/usr/bin/env python3
"""Verify SSH, SFTP and the RFB desktop against a disposable LiteOS image."""
from __future__ import annotations

import argparse
import os
import shlex
import shutil
import socket
import subprocess
import tempfile
import time
from pathlib import Path

from build_target import target_from_environment
from qemu_gate import _qemu_command, terminate
from runtime_debug import deploy, desktop
from verify_wayland import install_files

ROOT = Path(__file__).resolve().parent.parent
LOG = ROOT / "target/logs/diagnostics-serial.log"


def verify(image: Path, directory: Path) -> None:
    private = directory / "fs.img"
    shutil.copyfile(image, private)
    for identity in ("operator", "host"):
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", directory / identity], check=True)
    install_files(private, [
        (directory / "operator.pub", "/root/.ssh/authorized_keys"),
        (directory / "host", "/etc/ssh/ssh_host_ed25519_key"),
    ], directory)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    known = directory / "known_hosts"
    known.write_text(f"[127.0.0.1]:{port} " + (directory / "host.pub").read_text())
    config = directory / "ssh_config"
    config.write_text(
        f"Host liteos-test\nHostName 127.0.0.1\nUser root\nPort {port}\n"
        f"IdentityFile {directory}/operator\nIdentitiesOnly yes\nUserKnownHostsFile {known}\n"
        "StrictHostKeyChecking yes\nBatchMode yes\nConnectTimeout 2\n"
    )
    shims = directory / "bin"
    shims.mkdir()
    for name in ("ssh", "scp"):
        executable = shutil.which(name)
        if executable is None:
            raise RuntimeError(f"{name} is required")
        shim = shims / name
        shim.write_text(f'#!/bin/sh\nexec {shlex.quote(executable)} -F {shlex.quote(str(config))} "$@"\n')
        shim.chmod(0o700)
    os.environ["PATH"] = str(shims) + os.pathsep + os.environ["PATH"]
    argv = _qemu_command(private, 4, memory="2G", display_qmp=directory / "qmp")
    argv[argv.index("user,id=net0")] += f",hostfwd=tcp:127.0.0.1:{port}-:22"
    print(shlex.join(argv), flush=True)
    LOG.parent.mkdir(parents=True, exist_ok=True)
    with LOG.open("wb") as serial:
        process = subprocess.Popen(argv, cwd=ROOT, stdin=subprocess.DEVNULL, stdout=serial,
                                   stderr=subprocess.STDOUT, start_new_session=True)
        try:
            run_checks(process, directory)
        finally:
            terminate(process)


def run_checks(process: subprocess.Popen, directory: Path) -> None:
    deadline = time.monotonic() + 45
    while time.monotonic() < deadline:
        result = subprocess.run(["ssh", "liteos-test", "id -u"], capture_output=True, timeout=6)
        if result.returncode == 0 and result.stdout.strip() == b"0":
            break
        if process.poll() is not None:
            raise RuntimeError("QEMU exited before SSH was ready")
        time.sleep(0.25)
    else:
        raise RuntimeError(f"SSH not ready: {result.stderr.decode(errors='replace')}")
    print("SSH command passed", flush=True)
    rejected = subprocess.run(["ssh", "-oPubkeyAuthentication=no", "liteos-test", "true"], capture_output=True)
    if rejected.returncode == 0 or b"Permission denied" not in rejected.stderr:
        raise RuntimeError("SSH did not reject a connection without a public key")
    while "compositor: boot complete" not in LOG.read_text(errors="replace"):
        if time.monotonic() >= deadline or process.poll() is not None:
            raise RuntimeError("desktop did not become ready")
        time.sleep(0.1)
    application = directory / "application"
    destination = "/root/diagnostic-test"
    for version in ("first", "replacement"):
        application.write_text(f"#!/bin/sh\nprintf '%s\\n' {version}\n")
        deploy("liteos-test", application, destination, launch=version == "replacement")
        result = subprocess.check_output(["ssh", "liteos-test", destination], text=True)
        if result.strip() != version:
            raise RuntimeError("the executable deployed over SFTP did not run the expected version")
    launched = subprocess.check_output(["ssh", "liteos-test", "cat /root/.liteos-launch.log"], text=True)
    if launched.strip() != "replacement":
        raise RuntimeError("the published application did not start")
    print("SFTP executable replacement and launch passed", flush=True)
    before = directory / "before.ppm"
    after = directory / "after.ppm"
    desktop("liteos-test", "screen", output=before)
    pixels = before.read_bytes().split(b"\n", 3)[-1]
    if len(set(pixels)) < 8:
        raise RuntimeError("RFB returned a blank desktop")
    desktop("liteos-test", "pointer", position=(21, 785), button=1)
    desktop("liteos-test", "key", keys=(0xff1b,))
    serial = LOG.read_text(errors="replace")
    if "compositor: first pointer event" not in serial or "compositor: first key event" not in serial:
        raise RuntimeError("RFB input did not reach the compositor seat")
    desktop("liteos-test", "key", keys=(0xffeb, ord("r")))
    desktop("liteos-test", "screen", output=after)
    if before.read_bytes() == after.read_bytes():
        raise RuntimeError("remote input did not change the desktop frame")
    print("SSH-forwarded RFB frame and input passed", flush=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", type=Path, default=ROOT / "target/rootfs/aarch64.img")
    args = parser.parse_args()
    if target_from_environment().arch != "aarch64":
        return 0
    with tempfile.TemporaryDirectory(prefix="liteos-diagnostics-", dir="/tmp") as temporary:
        verify(args.image, Path(temporary))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
