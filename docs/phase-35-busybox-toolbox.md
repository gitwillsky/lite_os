# Phase 35：BusyBox text and archive toolbox

## 目标与单一路径

默认 rootfs 继续只有一个 BusyBox ELF inode。新增 applet 只通过唯一 `user/busybox.config` 进入产物，并由 rootfs builder 建立指向 `/bin/init` 的 hardlink；不存在独立二进制、shell builtin 替代路径或 BusyBox source patch。

本阶段实际启用并运行：

- 文本：`awk`、`sed`、`head`、`tail`、`cut`、`sort`、`uniq`、`tr`、`tee`；
- 路径/批处理：`find`、`basename`、`dirname`、`expr`、`seq`、`sleep`；
- 归档/校验：`gzip`、`gunzip`、`zcat`、`sha256sum`。

## utimensat

已有 config 中的 `touch` 首次进入真实 gate 后暴露 `utimensat(88)` 缺失。实现沿 VFS 唯一 pathname/dirfd resolver 到 Inode mutation seam，支持 null times、两个 RV64 timespec、`UTIME_NOW/UTIME_OMIT` 与 `AT_SYMLINK_NOFOLLOW`。ext2 在 filesystem mutation lock 内一次写入 atime/mtime/ctime；devfs 通过同一 Inode seam 返回 `EROFS`。

ext2 revision 1 inode 只保存 32-bit epoch seconds：显式值或 realtime 超界返回 `EOVERFLOW`，不截断；纳秒输入完成语法校验但磁盘精度明确为秒。当前固定 root identity 不做 permission check，`AT_EMPTY_PATH` 仍拒绝。

## 真实 gate

UART ash 依次证明：

1. `sort | uniq | wc` 与 `sed | cut | tr`，并由 `awk/head/tail/tee` 交叉核对内容；
2. `touch` 创建并更新时间、`find` 遍历、路径与算术工具输出；
3. gzip round-trip 经 `zcat/gunzip` 恢复原文，`sha256sum` 匹配固定 digest；
4. 所有 applet 与 init 是同一 inode，link count 与目录项完整。

启动围栏现在禁止任意 `unsupported syscall_id:`，不再只枚举已知编号。

## 明确未启用

- `yes` 的无限 writer 在 `head` 提前关闭 reader 后暴露 pipe/exec signal lifecycle 阻塞；未以 timeout 或有限输出伪装支持。
- `xargs` 的 spawn 路径需要当前 clone 尚未实现的 `CLONE_VFORK`；未回退成 shell wrapper。

二者均未进入 config、hardlink rootfs 或支持声明，后续应分别按 pipe close/SIGPIPE 与 vfork process lifecycle 竖切实现。
