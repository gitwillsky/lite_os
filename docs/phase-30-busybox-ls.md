# Phase 30：BusyBox ls 与 `AT_SYMLINK_NOFOLLOW`

## 症状与根因

BusyBox `ls` 能通过 `getdents64` 读到 `lost+found/bin/etc`，但对每个条目报 `Invalid argument`。原因是 `ls` 使用 `lstat`，musl 将其编译为 `newfstatat(..., AT_SYMLINK_NOFOLLOW)`，而 kernel 原实现拒绝任何非零 flags。因此目录记录布局和 ext2 metadata 都不是故障点。

## 单一修复路径

`newfstatat` 现在接受 Linux `AT_SYMLINK_NOFOLLOW`。VFS 在唯一 pathname resolver 内区分末项：无尾随 `/` 的最后一个 symlink 可以返回 link inode 自身；中间 symlink 和要求必须是目录的尾随 `/` 仍走现有 `ELOOP/ENOTDIR` 边界。没有忽略 flag、BusyBox patch 或另一个 stat path。

## 可执行证据

BusyBox gate 进入真实 UART ash 后执行 `/bin/ls /`，只有命令完成后才能通过算术生成 `LITEOS_LS_42`，并把 `Invalid argument` 设为 forbidden marker。这个反馈环精确覆盖用户截图中的失败模式。
