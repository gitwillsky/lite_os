# Phase 54：BusyBox 交互文本与诊断工具箱

本阶段在唯一动态 BusyBox 1.37.0 rootfs 上开放 `vi/less/more`、`diff/patch`、`hexdump/hd/od/strings` 与 `clear/reset`。所有 applet 仍是 `/bin/init` 同一 inode 的 hardlink，不引入独立 editor、pager、source patch 或第二条 userspace 路径。

## 实现边界

- `vi` 启用常用 colon、search、mark、undo、terminal resize 与 readonly 能力；单缓冲区最大文本长度固定为 4096 bytes，不声明完整 Vim 兼容。
- `less` 支持 regexp、mark、line number、window resize 与 raw control character；`more` 保留基础 pager 语义。
- `patch` 的已打开目标 metadata 更新使用标准 Linux/riscv64 `fchmod(52)`；fd 与 pathname chmod 共用同一权限和 set-id 处理，不存在 BusyBox 特判。
- QEMU UART gate 只对包含 ESC 的 raw-mode interaction 逐字节注入并在 ESC 边界等待 guest mode transition；普通 shell 输入继续分批发送。

## 运行验收事实

- guest 以 `vi` 编辑并写回 ext2 文件，退出后通过 `stty` 核对 echo 已恢复。
- guest 以 `less` 分页 40 行文件并退出，随后同样核对 terminal state；`more` 经 pipe 输出真实文件。
- guest 生成 unified diff，经 `patch` 修改目标，再由 `cmp` 核对；`hexdump` 与 `od` 核对字节，`strings` 扫描动态 BusyBox ELF。
- 完整 `python3 scripts/verify_busybox.py --image fs.img` 冷启动门通过，未出现未支持 syscall。
