# Phase 28：BusyBox 持久化与 system reset

## 目标与所有权

BusyBox `init` 通过 Linux `reboot(142)` 管理 Ctrl-Alt-Delete policy；kernel `system` module 是该 policy 与 whole-system reset/shutdown 的唯一 owner。syscall 层只验证 Linux magic/command，重启和关机只经 SBI SRST seam，不恢复已删除的私有 power syscall 或第二条 platform 路径。

ext2 持久化证据使用同一个镜像：1-hart BusyBox ash 通过标准 open/write/close 写入 `/persist`，`sync` applet 触发 `sync(81)` 并等待 VirtIO block flush；gate 终止第一个 QEMU 后，用 8-hart 冷启动同一镜像，再由 BusyBox `cat` 读回固定 marker。

## 精确边界

- `CAD_OFF/CAD_ON` 更新真实原子 policy，为未来 input IRQ 与 syscall 并发预留唯一语义；当前 QEMU virt 无 Ctrl-Alt-Delete input device。
- `RESTART/HALT/POWER_OFF` 分别映射 SBI cold reboot/shutdown；SRST 成功按规范不返回，若 firmware 意外返回则 syscall 报 `EIO`。
- rootfs 直接提供 BusyBox `halt/poweroff/reboot`；BusyBox 上游没有 `shutdown` applet，产品 `/bin/shutdown` 只解析立即动作并委托给上游 applet，不复制 reset ABI 或 system policy。
- 普通 reboot/poweroff 由 BusyBox init 先 TERM、sync、等待、KILL、再 reset；两段固定一秒 grace period 是用户态优雅退出语义。`reboot -f` 只用于明确跳过 init 清理的快速开发重启。
- platform 无 restart-reason channel，因此 `RESTART2`、kexec 和 suspend 不伪成功。
- ext2 仍无 journal；证据覆盖显式 sync 后的冷启动保存，不声称突然断电的跨块事务原子性。

## 验收证据

BusyBox gate 严禁 `reboot(142)` 落入 unsupported path，并要求第一次启动输出 `LITEOS_PERSIST_WRITTEN_42`、第二次启动输出磁盘文件中的 `LITEOS_PERSIST_42`。第二次同时验证 DTB 动态 topology 的 8-hart online mask `0xff`。
