#!/usr/bin/env python3
"""Generate and control LiteOS's single macOS UTM GUI runtime."""

from __future__ import annotations

import os
import plistlib
import shutil
import socket
import subprocess
import time
from pathlib import Path
from typing import BinaryIO

UTM_APP = Path("/Applications/UTM.app")
UTM_VERSION = "4.7.5"
UTMCTL = UTM_APP / "Contents/MacOS/utmctl"
UTM_INFO = UTM_APP / "Contents/Info.plist"
UTM_DOCUMENTS = Path.home() / "Library/Containers/com.utmapp.UTM/Data/Documents"
UTM_ARTIFACTS = (
    Path.home()
    / "Library/Group Containers/WDNLXAD4W8.com.utmapp.UTM/LiteOS"
)
VM_NAME = "LiteOS"
VM_UUID = "11E05A11-7E05-4A11-8E7A-11E05A110001"
VM_PACKAGE = UTM_ARTIFACTS / f"{VM_NAME}.utm"
VM_DOCUMENT_PACKAGE = UTM_DOCUMENTS / f"{VM_NAME}.utm"


def _installed_version() -> str:
    if not UTM_INFO.is_file() or not UTMCTL.is_file():
        raise RuntimeError(
            f"UTM {UTM_VERSION} is required at {UTM_APP}; "
            "install the pinned release from https://github.com/utmapp/UTM/releases/tag/v4.7.5"
        )
    with UTM_INFO.open("rb") as stream:
        value = plistlib.load(stream).get("CFBundleShortVersionString")
    if value != UTM_VERSION:
        raise RuntimeError(
            f"UTM {UTM_VERSION} is required; {UTM_APP} contains {value or 'an unknown version'}"
        )
    return value


def _memory_mib(value: str) -> int:
    normalized = value.strip().upper()
    factors = {"M": 1, "G": 1024}
    if len(normalized) < 2 or normalized[-1] not in factors:
        raise ValueError(f"UTM memory must use M or G units; got {value!r}")
    amount = int(normalized[:-1])
    if amount <= 0:
        raise ValueError("UTM memory must be positive")
    return amount * factors[normalized[-1]]


def _publish_file(source: Path, destination: Path) -> None:
    """Publish one artifact as a hard link so UTM and workflow share one inode."""
    source = source.resolve(strict=True)
    destination.parent.mkdir(parents=True, exist_ok=True)
    if destination.exists():
        source_stat = source.stat()
        destination_stat = destination.stat()
        if (source_stat.st_dev, source_stat.st_ino) == (
            destination_stat.st_dev,
            destination_stat.st_ino,
        ):
            return
        destination.unlink()
    try:
        os.link(source, destination)
    except OSError as error:
        raise RuntimeError(
            f"UTM artifact publication requires one APFS volume: {source} -> {destination}"
        ) from error


def _qemu_path(path: Path) -> str:
    """Encode a host path for QEMU's comma-separated key/value parser."""
    return str(path).replace(",", ",,")


def _configuration(
    *,
    memory_mib: int,
    cpu_count: int,
    kernel: Path | None = None,
    rootfs: Path | None = None,
    serial_tcp_port: int | None = None,
    qmp_socket: Path | None = None,
    audio_output: Path | None = None,
) -> dict[str, object]:
    kernel = kernel or UTM_ARTIFACTS / "kernel"
    rootfs = rootfs or UTM_ARTIFACTS / "rootfs.img"
    # UTM's generated SPICE server enables vdagent mouse delivery by default.
    # Disable it explicitly so the attached virtio-tablet remains the sole
    # pointer source; otherwise SPICE diverts host motion into mouse-state
    # messages while the guest's canonical evdev tablet stays permanently idle.
    audio_id = "audio0"
    additional = [
        "-global",
        "virtio-mmio.force-legacy=false",
        "-global",
        "virtio-gpu-device.xres=3008",
        "-global",
        "virtio-gpu-device.yres=1692",
        "-spice",
        "agent-mouse=off",
        "-rtc",
        "base=utc",
        "-drive",
        '"'
        + (
            "if=none,media=disk,id=driverootfs,format=raw,"
            f"file.filename={_qemu_path(rootfs)},discard=unmap,detect-zeroes=unmap"
        )
        + '"',
        "-kernel",
        f'"{kernel}"',
        "-device",
        "virtio-blk-device,drive=driverootfs",
        "-object",
        "rng-random,filename=/dev/urandom,id=rng0",
        "-device",
        "virtio-rng-device,rng=rng0",
        "-device",
        "virtio-keyboard-device",
        "-device",
        "virtio-tablet-device",
    ]
    if qmp_socket is not None:
        additional.extend(
            (
                "-qmp",
                f'"unix:{_qemu_path(qmp_socket)},server=on,wait=off"',
            )
        )
    if audio_output is not None:
        audio_id = "gate-audio"
        additional.extend(
            (
                "-audiodev",
                f'"wav,id={audio_id},path={_qemu_path(audio_output)},'
                'out.frequency=48000,out.channels=2,out.format=s16"',
            )
        )
    additional.extend(
        (
            "-device",
            f"virtio-sound-device,id=audio-device,audiodev={audio_id},streams=1",
        )
    )
    serial = (
        [{"Mode": "Ptty", "Target": "Auto"}]
        if serial_tcp_port is None
        else [
            {
                # OWNER: the gate listens before UTM starts, so QEMU connects without
                # losing early boot markers. TcpServer+WaitForConnection blocks UTM's
                # own QMP startup and leaves an otherwise-running VM stuck at `starting`.
                "Mode": "TcpClient",
                "Target": "Auto",
                "TcpHostAddress": "127.0.0.1",
                "TcpPort": serial_tcp_port,
            }
        ]
    )
    return {
        "Backend": "QEMU",
        "ConfigurationVersion": 4,
        "Information": {
            "Name": VM_NAME,
            "Icon": "linux",
            "IconCustom": False,
            "Notes": "Generated by LiteOS; edit scripts/utm_runtime.py, not this VM.",
            "UUID": VM_UUID,
        },
        "System": {
            "Architecture": "aarch64",
            "Target": "virt",
            "CPU": "host",
            "CPUFlagsAdd": [],
            "CPUFlagsRemove": [],
            "CPUCount": cpu_count,
            "ForceMulticore": False,
            "MemorySize": memory_mib,
            "JITCacheSize": 0,
        },
        "QEMU": {
            "DebugLog": True,
            "UEFIBoot": False,
            "RNGDevice": False,
            "BalloonDevice": False,
            "TPMDevice": False,
            "Hypervisor": True,
            "TSO": False,
            "RTCLocalTime": False,
            "PS2Controller": False,
            "MachinePropertyOverride": (
                "gic-version=3,its=off,secure=off,virtualization=off,acpi=off,"
                "highmem-ecam=off"
            ),
            "AdditionalArguments": additional,
        },
        "Input": {
            "UsbBusSupport": "Disabled",
            "UsbSharing": False,
            "MaximumUsbShare": 0,
        },
        "Sharing": {
            "DirectoryShareMode": "None",
            "DirectoryShareReadOnly": False,
            "ClipboardSharing": True,
        },
        "Display": [
            {
                "Hardware": "virtio-gpu-gl-device",
                "DynamicResolution": True,
                "UpscalingFilter": "Linear",
                "DownscalingFilter": "Linear",
                "NativeResolution": True,
            }
        ],
        "Drive": [],
        "Network": [
            {
                "Mode": "Emulated",
                "Hardware": "virtio-net-device",
                "MacAddress": "02:4C:49:54:45:01",
                "IsolateFromHost": False,
                "PortForward": [],
            }
        ],
        "Serial": serial,
        "Sound": [],
    }


def prepare(
    *,
    kernel: Path,
    rootfs: Path,
    memory: str,
    cpu_count: int,
    serial_tcp_port: int | None = None,
    qmp_socket: Path | None = None,
    audio_output: Path | None = None,
) -> Path:
    """Publish current artifacts and an exact UTM v4 configuration."""
    _installed_version()
    if cpu_count <= 0:
        raise ValueError("UTM CPU count must be positive")
    UTM_ARTIFACTS.mkdir(parents=True, exist_ok=True)
    published_rootfs = UTM_ARTIFACTS / "rootfs.img"
    published_kernel = UTM_ARTIFACTS / "kernel"
    _publish_file(rootfs, published_rootfs)
    _publish_file(kernel, published_kernel)
    VM_PACKAGE.mkdir(parents=True, exist_ok=True)
    config = _configuration(
        memory_mib=_memory_mib(memory),
        cpu_count=cpu_count,
        kernel=published_kernel,
        rootfs=published_rootfs,
        serial_tcp_port=serial_tcp_port,
        qmp_socket=qmp_socket,
        audio_output=audio_output,
    )
    with (VM_PACKAGE / "config.plist").open("wb") as stream:
        plistlib.dump(config, stream, fmt=plistlib.FMT_XML, sort_keys=False)
        stream.flush()
        os.fsync(stream.fileno())
    return VM_PACKAGE


def _ctl(*arguments: str) -> str:
    try:
        result = subprocess.run(
            [str(UTMCTL), *arguments],
            check=True,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=30,
        )
    except subprocess.TimeoutExpired as error:
        raise RuntimeError(f"utmctl {' '.join(arguments)} did not complete") from error
    return result.stdout.strip()


def _registered() -> bool:
    return _status() is not None


def _status() -> str | None:
    """Return status from one registry snapshot without querying a stale VM handle."""
    for line in _ctl("list").splitlines()[1:]:
        fields = line.split(maxsplit=2)
        if len(fields) == 3 and fields[0] == VM_UUID:
            return fields[1]
    return None


def _apple_string(value: str) -> str:
    return value.replace("\\", "\\\\").replace('"', '\\"')


def _start_visible() -> None:
    """Start through UTM's public scripting API so the Metal display window is owned by UTM."""
    script = (
        'tell application id "com.utmapp.UTM"\n'
        f'  set vm to first virtual machine whose id is "{VM_UUID}"\n'
        "  start vm\n"
        "end tell"
    )
    try:
        subprocess.run(
            ["osascript", "-e", script],
            check=True,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=30,
        )
    except subprocess.TimeoutExpired as error:
        raise RuntimeError("UTM did not create the LiteOS display window") from error


def _ensure_registered() -> None:
    subprocess.run(["open", "-a", str(UTM_APP)], check=True)
    for _ in range(40):
        try:
            if _registered():
                return
            break
        except (subprocess.CalledProcessError, RuntimeError):
            time.sleep(0.25)
    else:
        raise RuntimeError("UTM did not become available for managed VM registration")
    package = _apple_string(str(VM_PACKAGE))
    script = (
        'tell application id "com.utmapp.UTM"\n'
        f'  import new virtual machine from POSIX file "{package}"\n'
        "end tell"
    )
    subprocess.run(
        ["osascript", "-e", script],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=30,
    )
    for _ in range(40):
        if _registered():
            return
        time.sleep(0.25)
    raise RuntimeError(f"UTM did not register generated VM at {VM_PACKAGE}")


def _remove_registered() -> None:
    """Remove only the generated VM registration before publishing its next config."""
    subprocess.run(["open", "-a", str(UTM_APP)], check=True)
    for _ in range(40):
        try:
            status = _status()
            if status is None:
                if VM_DOCUMENT_PACKAGE.exists():
                    shutil.rmtree(VM_DOCUMENT_PACKAGE)
                return
            if status != "stopped":
                raise RuntimeError(
                    f"UTM VM {VM_NAME!r} must be stopped before regeneration; status={status}"
                )
            _ctl("delete", VM_UUID)
            if VM_DOCUMENT_PACKAGE.exists():
                shutil.rmtree(VM_DOCUMENT_PACKAGE)
            return
        except subprocess.CalledProcessError:
            time.sleep(0.25)
    raise RuntimeError("UTM did not become available for managed VM regeneration")


def _stop_managed() -> None:
    """Stop only the generated LiteOS VM and wait for QEMU ownership to end."""
    current = _status()
    if current in (None, "stopped"):
        return
    _ctl("stop", "--hide", VM_UUID)
    for _ in range(40):
        if _status() in (None, "stopped"):
            return
        time.sleep(0.25)
    # UTM documents a second stop as the force-stop edge for an unresponsive VM.
    _ctl("stop", "--hide", VM_UUID)
    for _ in range(40):
        if _status() in (None, "stopped"):
            return
        time.sleep(0.25)
    raise RuntimeError("UTM did not stop the managed LiteOS gate VM")


class GateRuntime:
    """One disposable UTM VirGL runtime with observable serial and optional QMP."""

    def __init__(
        self,
        serial_socket: socket.socket,
        *,
        kernel: Path,
        rootfs: Path,
        memory: str,
        cpu_count: int,
        qmp_socket: Path | None,
        audio_output: Path | None,
        restore: tuple[bytes | None, Path | None, Path | None],
    ) -> None:
        self._serial_socket = serial_socket
        self.stdout: BinaryIO = serial_socket.makefile("rb", buffering=0)
        self.kernel = kernel
        self.rootfs = rootfs
        self.memory = memory
        self.cpu_count = cpu_count
        self.qmp_socket = qmp_socket
        self.audio_output = audio_output
        self._restore = restore
        self._closed = False

    def poll(self) -> int | None:
        """Return `None` while UTM owns a running QEMU process, otherwise zero."""
        return None if _status() in ("starting", "started") else 0

    def wait(self, timeout: float) -> int:
        """Wait for the disposable QEMU process to stop within `timeout` seconds."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            result = self.poll()
            if result is not None:
                return result
            time.sleep(0.1)
        raise subprocess.TimeoutExpired("UTM LiteOS gate", timeout)

    def close(self) -> None:
        """Stop the gate and restore the generated development VM configuration."""
        if self._closed:
            return
        self._closed = True
        _stop_managed()
        self.stdout.close()
        self._serial_socket.close()
        _remove_registered()
        _restore_after_gate(
            self._restore,
            kernel=self.kernel,
            rootfs=self.rootfs,
            memory=self.memory,
            cpu_count=self.cpu_count,
        )
        _ensure_registered()
        if self.qmp_socket is not None:
            self.qmp_socket.unlink(missing_ok=True)


def _backup_for_gate() -> tuple[bytes | None, Path | None, Path | None]:
    """Preserve the exact generated development VM while a gate is disposable."""
    config_path = VM_PACKAGE / "config.plist"
    config = config_path.read_bytes() if config_path.is_file() else None
    backups: list[Path | None] = []
    for name in ("kernel", "rootfs.img"):
        source = UTM_ARTIFACTS / name
        backup = UTM_ARTIFACTS / f".gate-backup-{name}-{os.getpid()}"
        backup.unlink(missing_ok=True)
        if source.is_file():
            os.link(source, backup)
            backups.append(backup)
        else:
            backups.append(None)
    return config, backups[0], backups[1]


def _restore_after_gate(
    restore: tuple[bytes | None, Path | None, Path | None],
    *,
    kernel: Path,
    rootfs: Path,
    memory: str,
    cpu_count: int,
) -> None:
    """Restore the pre-gate VM config and published artifact inodes exactly."""
    config, kernel_backup, rootfs_backup = restore
    if config is None or kernel_backup is None or rootfs_backup is None:
        prepare(
            kernel=kernel,
            rootfs=rootfs,
            memory=memory,
            cpu_count=cpu_count,
        )
        for backup in (kernel_backup, rootfs_backup):
            if backup is not None:
                backup.unlink(missing_ok=True)
        return
    _publish_file(kernel_backup, UTM_ARTIFACTS / "kernel")
    _publish_file(rootfs_backup, UTM_ARTIFACTS / "rootfs.img")
    config_path = VM_PACKAGE / "config.plist"
    with config_path.open("wb") as stream:
        stream.write(config)
        stream.flush()
        os.fsync(stream.fileno())
    kernel_backup.unlink()
    rootfs_backup.unlink()


def start_gate(
    *,
    kernel: Path,
    rootfs: Path,
    memory: str = "2G",
    cpu_count: int = 1,
    qmp: bool = False,
    capture_audio: bool = False,
) -> GateRuntime:
    """Start the sole product VirGL path as a hidden disposable UTM runtime."""
    _installed_version()
    kernel = kernel.resolve(strict=True)
    rootfs = rootfs.resolve(strict=True)
    serial_listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    serial_listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    serial_listener.bind(("127.0.0.1", 0))
    serial_listener.listen(1)
    serial_port = int(serial_listener.getsockname()[1])
    qmp_socket = (
        UTM_ARTIFACTS / f"gate-qmp-{os.getpid()}.sock" if qmp else None
    )
    audio_output = UTM_ARTIFACTS / "gate-audio.wav" if capture_audio else None
    for path in (qmp_socket, audio_output):
        if path is not None:
            path.unlink(missing_ok=True)
    restore = _backup_for_gate()
    _remove_registered()
    try:
        prepare(
            kernel=kernel,
            rootfs=rootfs,
            memory=memory,
            cpu_count=cpu_count,
            serial_tcp_port=serial_port,
            # QEMUHelper is sandboxed to UTM's app group and starts with that
            # directory as cwd. A short relative path satisfies both the sandbox
            # and macOS's 104-byte AF_UNIX limit; an absolute app-group path does not.
            qmp_socket=(
                qmp_socket.relative_to(UTM_ARTIFACTS.parent)
                if qmp_socket
                else None
            ),
            audio_output=audio_output,
        )
        _ensure_registered()
        if _status() != "stopped":
            raise RuntimeError("UTM gate VM must be stopped before launch")
        _ctl("start", "--hide", VM_UUID, "--disposable")
        serial_listener.settimeout(15.0)
        try:
            serial_socket, _ = serial_listener.accept()
        except TimeoutError as error:
            raise RuntimeError("UTM gate serial endpoint did not become ready") from error
        serial_socket.setblocking(False)
        return GateRuntime(
            serial_socket,
            kernel=kernel,
            rootfs=rootfs,
            memory=memory,
            cpu_count=cpu_count,
            qmp_socket=qmp_socket,
            audio_output=audio_output,
            restore=restore,
        )
    except BaseException:
        _stop_managed()
        _remove_registered()
        _restore_after_gate(
            restore,
            kernel=kernel,
            rootfs=rootfs,
            memory=memory,
            cpu_count=cpu_count,
        )
        _ensure_registered()
        raise
    finally:
        serial_listener.close()


def run_gui(
    *,
    kernel: Path,
    rootfs: Path,
    memory: str,
    cpu_count: int,
) -> None:
    """Start the generated VM and keep the invoking terminal as lifecycle owner."""
    _installed_version()
    _remove_registered()
    prepare(kernel=kernel, rootfs=rootfs, memory=memory, cpu_count=cpu_count)
    _ensure_registered()
    status = _status()
    if status != "stopped":
        raise RuntimeError(f"UTM VM {VM_NAME!r} must be stopped before launch; status={status}")
    _start_visible()
    for _ in range(40):
        status = _status()
        if status is None:
            raise RuntimeError("UTM removed the managed LiteOS registration during launch")
        if status == "started":
            break
        time.sleep(0.25)
    else:
        raise RuntimeError(f"UTM failed to start {VM_NAME}")
    try:
        while True:
            status = _status()
            if status == "stopped":
                return
            if status is None:
                raise RuntimeError("UTM removed the managed LiteOS registration while running")
            time.sleep(0.5)
    except KeyboardInterrupt:
        if _registered():
            _ctl("stop", VM_UUID)
        return
