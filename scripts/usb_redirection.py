"""一次 GUI 会话的标准 usbredir 连接及管理员 USB 捕获租约。"""
from __future__ import annotations

import socket
import subprocess

from utm_build import ROOT, USB_BRIDGE, USB_REDIRECT


class UsbRedirection:
    """workflow 拥有控制管道；root supervisor 的寿命不能超过本次 GUI 会话。"""

    def __init__(self) -> None:
        self.reservations: list[socket.socket] = []
        self.ports: list[int] = []
        self.process: subprocess.Popen | None = None
        self.log = None
        try:
            # 与原 UTM USB 分享容量一致；端口只绑定 loopback，不向局域网暴露设备。
            for _ in range(3):
                reservation = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
                reservation.bind(("127.0.0.1", 0))
                self.reservations.append(reservation)
                self.ports.append(reservation.getsockname()[1])
        except BaseException:
            self.close()
            raise

    def arguments(self) -> list[str]:
        """生成 QEMU 原生 socket chardev 与 usb-redir；不经过无捕获权限的 UTM 前端。"""
        result = []
        for index, port in enumerate(self.ports):
            result.extend((
                "-chardev", f"socket,id=usbredirchardev{index},host=127.0.0.1,port={port},server=on,wait=off",
                "-device", f"usb-redir,chardev=usbredirchardev{index},id=usbredirdev{index},bus=usb-controller-0.0",
            ))
        return result

    def authorize(self) -> None:
        """仅通过本机 sudo 的终端提示授权；不读取、缓存或记录管理员密码。"""
        print("USB DAC forwarding requires administrator authorization for this GUI session.", flush=True)
        subprocess.run(["sudo", "-v"], check=True)

    def release_ports(self) -> None:
        """仅在启动 QEMU 前释放预留端口；QEMU 绑定失败时不得启动 root 转发。"""
        for reservation in self.reservations:
            reservation.close()
        self.reservations.clear()

    def start(self) -> None:
        """QEMU started 后才允许 root 进程连接已绑定的端口。"""
        path = ROOT / "target/logs/usb-redirection.log"
        path.parent.mkdir(parents=True, exist_ok=True)
        self.log = path.open("wb", buffering=0)
        self.process = subprocess.Popen(
            ["sudo", "-n", "--", str(USB_BRIDGE), str(USB_REDIRECT), *map(str, self.ports)],
            stdin=subprocess.PIPE, stdout=self.log, stderr=subprocess.STDOUT,
        )

    def check(self) -> None:
        """捕获进程失败必须反馈给启动 owner，避免桌面运行但 DAC 静默失效。"""
        if self.process is not None and self.process.poll() is not None:
            detail = (ROOT / "target/logs/usb-redirection.log").read_text(errors="replace")[-2000:]
            raise RuntimeError(f"USB forwarding stopped ({self.process.returncode}):\n{detail}")

    def close(self) -> None:
        """关闭唯一控制 writer，让 root supervisor 释放所有设备并回收转发子进程。"""
        self.release_ports()
        try:
            if self.process is not None:
                if self.process.stdin is not None:
                    self.process.stdin.close()
                # 每个设备最多 3 秒释放，另加一次扫描和 sudo monitor 的退出时间。
                self.process.wait(timeout=3 * len(self.ports) + 3)
        finally:
            if self.log is not None:
                self.log.close()
