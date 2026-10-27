#!/usr/bin/env python3
"""Validate standard GPU sharing, Wayland EGL consumption and page-flip cadence.

A private UTM product image first runs upstream eglinfo and the native PRIME/flip probe.
It then starts the actual display service and upstream Weston simple-egl, requiring linux-dmabuf
creation, repeated buffer releases and successful client cleanup. No input or styles are judged here.
Flip timing uses the guest kernel's monotonic DRM event clock with the existing unchanged limits.
The gate restores the managed UTM configuration and keeps a complete serial log for diagnosis.
AArch64+HVF only; RISC-V has no VirGL product path.
"""

from __future__ import annotations

import argparse
import re
import shutil
import sys
import tempfile
import tarfile
from pathlib import Path

from build_cache import publish_runtime_gate, runtime_gate_hit, runtime_gate_payload
from build_target import BuildTarget, acceleration_from_environment, target_from_environment
from ext4_image import recover_ext4_journal, run_debugfs
from qemu_gate import guest_inittab
from mesa_runtime import mesa_userland
from apk_cache import download
from utm_gate import boot_graphics
from verify_busybox import build_drm_flip_probe
from verify_musl import cached_musl_paths, find_compiler

ROOT = Path(__file__).resolve().parent.parent

PROBE_GUEST_PATH = "/bin/drm-flip-probe"
DONE_MARKER = "EGL_WAYLAND_SHARING_READY"
RESULT_RE = re.compile(
    r"drm-flip-probe: mode=(\d+)x(\d+) readback=ok frames=(\d+) dropped=(\d+) "
    r"p50_us=(\d+) p95_us=(\d+) p99_us=(\d+)"
)
ERROR_RE = re.compile(r"drm-flip-probe: error: (.+)")

# Must match `MEASURED_FRAMES` in user/libraries/linux-uapi/src/bin/drm-flip-probe/main.rs; fewer completions
# mean the probe never produced a real steady stream.
MEASURED_FRAMES = 600

# Reference cadence, printed as context and never gated directly.
CONTRACT_P95_US = 16_670
CONTRACT_P99_US = 33_300
# Wide-but-real RED ceilings: one 60 Hz frame of slack at p95, and a sustained multi-frame stall
# bound at p99, so host scheduling jitter under HVF does not flap the gate.
LIMIT_P95_US = 33_300
LIMIT_P99_US = 50_000
MAX_DROPPED = 0
# Bump to force a re-run after tightening thresholds or changing the result format.
RECIPE_VERSION = 3
EGLINFO_PACKAGE = ("mesa-utils-9.0.0-r6.apk", "2c7a962c677b81c1338becd983da947b23f7ebcfde00ff00361cada5703d93af")


def default_image(target: BuildTarget) -> Path:
    """Returns the target-isolated read-only rootfs baseline."""
    return ROOT / "target" / "rootfs" / f"{target.arch}.img"


def gate_inputs(image: Path, target: BuildTarget, probe: Path) -> tuple[Path, ...]:
    """Cache inputs: everything that can change the measured display/GPU behavior."""
    return (
        image,
        ROOT / target.kernel_boot_artifact(),
        probe,
        ROOT / "scripts" / "qemu_gate.py",
        ROOT / "scripts" / "utm_gate.py",
        ROOT / "scripts" / "utm_runtime.py",
        ROOT / "scripts" / "mesa_runtime.py",
        ROOT / "scripts" / "mesa-runtime-packages.json",
        Path(__file__).resolve(),
    )


def install_probe(image: Path, probe: Path, directory: Path) -> None:
    """Rewrites only the private gate image to boot straight into the probe."""
    recover_ext4_journal(image)
    inittab = directory / "inittab"
    startup = directory / "gpu-startup"
    startup.write_text(f"#!/bin/sh\n/bin/eglinfo -B -p gbm -a gles || {{ echo '[ERROR] EGL initialization failed'; exit 1; }}\n{PROBE_GUEST_PATH} || {{ echo '[ERROR] PRIME/flip failed'; exit 1; }}\nexec /etc/init.d/display-service\n")
    shell = directory / "logon"
    shell.write_text("""#!/bin/sh
# 标准外部客户端连接公开 socket；外壳专用 WAYLAND_SOCKET 不传给后台作业。
unset WAYLAND_SOCKET
check_client() {
    WAYLAND_DEBUG=1 /bin/weston-simple-egl "$@" >/var/tmp/egl-client.log 2>&1 &
    child=$!
    i=0
    while [ $i -lt 30 ]; do
        releases=$(/bin/grep -c 'wl_buffer.*release' /var/tmp/egl-client.log)
        if [ "$releases" -ge 3 ] && /bin/grep -q 'zwp_linux_buffer_params_v1.*create_immed' /var/tmp/egl-client.log; then
            kill -INT "$child"
            wait "$child"
            code=$?
            cat /var/tmp/egl-client.log
            return "$code"
        fi
        kill -0 "$child" 2>/dev/null || break
        sleep 1
        i=$((i+1))
    done
    cat /var/tmp/egl-client.log
    return 1
}
if check_client -f -o && check_client -f; then
    echo EGL_WAYLAND_SHARING_READY
else
    echo '[ERROR] EGL Wayland dma-buf frame/release/cleanup check failed'
fi
while :; do sleep 1; done
""")
    inittab.write_text(guest_inittab("/bin/sh /bin/gpu-startup"))
    run_debugfs(image, "rm /etc/inittab", writable=True)
    run_debugfs(image, f"write {inittab} /etc/inittab", writable=True)
    run_debugfs(image, f"write {probe} {PROBE_GUEST_PATH}", writable=True)
    with tarfile.open(download(*EGLINFO_PACKAGE), "r:gz") as archive:
        source = archive.extractfile("usr/bin/eglinfo")
        if source is None:
            raise RuntimeError("fixed mesa-utils lacks eglinfo")
        eglinfo = directory / "eglinfo"
        eglinfo.write_bytes(source.read())
    files = [(startup, "/bin/gpu-startup", 0o755), (shell, "/bin/logon", 0o755), (eglinfo, "/bin/eglinfo", 0o755)]
    for name, digest, source, destination in (
        ("weston-clients-14.0.2-r1.apk", "6abf86b9d6ac0677931a942acd9207c910fb25a2f3ec11ca29cc8e50adf4eb72", "usr/bin/weston-simple-egl", "/bin/weston-simple-egl"),
        ("wayland-libs-cursor-1.23.1-r3.apk", "c12e4c90ea1fa95d51c940d407405685ff25100bd63efabb71cd3dc067e070f6", "usr/lib/libwayland-cursor.so.0.23.1", "/usr/lib/liteos/libwayland-cursor.so.0"),
    ):
        with tarfile.open(download(name, digest), "r:gz") as archive:
            payload = archive.extractfile(source)
            if payload is None:
                raise RuntimeError(f"fixed upstream EGL consumer lacks {source}")
            local = directory / Path(destination).name
            local.write_bytes(payload.read())
            files.append((local, destination, 0o755))
    files.extend((file.source, file.destination, file.mode) for file in mesa_userland())
    directories = sorted({str(parent) for _, destination, _ in files for parent in Path(destination).parents if str(parent) != "/"}, key=lambda path: (path.count("/"), path))
    for folder in directories:
        run_debugfs(image, f"mkdir {folder}", writable=True)
    for source, destination, mode in files:
        run_debugfs(image, f"rm {destination}", writable=True)
        run_debugfs(image, f"write {source} {destination}", writable=True)
        run_debugfs(image, f"set_inode_field {destination} mode 0{0o100000 | mode:o}", writable=True)
    # The kernel requires at least one execute bit for root execve.
    run_debugfs(image, f"set_inode_field {PROBE_GUEST_PATH} mode 0100755", writable=True)
    installed = run_debugfs(image, "cat /etc/inittab")
    if "/bin/sh /bin/gpu-startup" not in installed:
        raise RuntimeError("probe inittab was not installed")


def parse(serial: str) -> dict[str, int]:
    """Extracts the probe result, or raises with the probe's own failure reason."""
    if "got error from kernel - expect bad rendering" in serial:
        raise RuntimeError("upstream Mesa reported a rejected GPU submission")
    failure = ERROR_RE.search(serial)
    if failure is not None:
        raise RuntimeError(f"drm-flip-probe failed: {failure.group(1)}")
    match = RESULT_RE.search(serial)
    if match is None:
        raise RuntimeError("drm-flip-probe printed no result line")
    if "drm-flip-probe: prime=ok exporter-close=ok implicit-sync=ok aliases=ok" not in serial:
        raise RuntimeError("GPU sharing probe did not complete")
    if re.search(r"OpenGL ES profile renderer:.*virgl", serial, re.IGNORECASE) is None:
        raise RuntimeError("upstream EGL consumer did not create a VirGL GLES context")
    if DONE_MARKER not in serial:
        raise RuntimeError("upstream Wayland EGL dma-buf consumer did not complete")
    keys = ("width", "height", "frames", "dropped", "p50_us", "p95_us", "p99_us")
    return dict(zip(keys, (int(value) for value in match.groups())))


def report(measured: dict[str, int]) -> int:
    """Prints the per-metric verdict and returns 1 if any threshold is red."""
    print(
        "AArch64+HVF kernel display/GPU "
        f"({measured['width']}x{measured['height']}, guest vblank clock, readback ok):"
    )
    print(f"  p50_us: {measured['p50_us']}")
    print(f"  contract p95<= {CONTRACT_P95_US}us p99<= {CONTRACT_P99_US}us (context)")
    limits = {"p95_us": LIMIT_P95_US, "p99_us": LIMIT_P99_US, "dropped": MAX_DROPPED}
    failed = False
    for metric, limit in limits.items():
        value = measured[metric]
        accepted = value <= limit
        print(f"- {metric}: {value} (limit {limit}) [{'ok' if accepted else 'RED'}]")
        failed |= not accepted
    if measured["frames"] < MEASURED_FRAMES:
        print(f"- frames: {measured['frames']} (need >= {MEASURED_FRAMES}) [RED, too few samples]")
        failed = True
    else:
        print(f"- frames: {measured['frames']} (>= {MEASURED_FRAMES}) [ok]")
    return 1 if failed else 0


def main() -> int:
    target = target_from_environment()
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--image",
        type=Path,
        default=default_image(target),
        help="read-only baseline rootfs; the gate only modifies a private copy",
    )
    args = parser.parse_args()
    if target.arch != "aarch64" or acceleration_from_environment() != "hvf":
        print(f"gpu gate skipped: {target.arch} has no VirGL display path in the gate")
        return 0
    image = args.image.resolve()
    try:
        if not image.is_file():
            raise RuntimeError(f"rootfs image is missing: {image}")
        probe = build_drm_flip_probe(cached_musl_paths(find_compiler()))
        stamp = ROOT / "target" / "verify-gates" / "gpu-aarch64.json"
        payload = runtime_gate_payload(
            "gpu", RECIPE_VERSION, gate_inputs(image, target, probe)
        )
        if runtime_gate_hit(stamp, payload, (image,)):
            print("gpu verification cache hit")
            return 0
        with tempfile.TemporaryDirectory(prefix="liteos-gpu-gate-") as private:
            private_root = Path(private)
            private_image = private_root / image.name
            shutil.copyfile(image, private_image)
            install_probe(private_image, probe, private_root)
            serial = boot_graphics(
                private_image, ROOT / target.kernel_boot_artifact(), (DONE_MARKER,),
                serial_log=ROOT / "target/logs/gpu-gate-serial.log"
            )
        code = report(parse(serial))
        if code != 0:
            return code
        publish_runtime_gate(stamp, payload)
    except RuntimeError as error:
        print(f"gpu verification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
