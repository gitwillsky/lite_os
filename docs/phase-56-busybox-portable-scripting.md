# Phase 56：BusyBox 可移植脚本与安装工具链

本阶段在唯一动态 BusyBox 1.37.0 rootfs 上开放外部 `test`/`[`/`[[`、`stat/mktemp/install/printenv/whoami/groups/yes` 与 `cksum/md5sum/sha1sum/sha512sum`，配合既有 `sha256sum` 形成 configure、临时工作区、staged install 与完整性校验竖切。所有 applet 仍是 `/bin/init` 同一 inode 的 hardlink，不引入 BusyBox patch 或第二套 userspace。

## Rootfs primitive

- 唯一 rootfs builder 固化 `01777 /tmp`、`0700 /root`、`/etc/passwd` 与 `/etc/group`；构建阶段直接检查 mode 和 canonical root identity records。
- root 与 nobody records 只服务当前单 user namespace 的标准 libc name lookup，不引入用户数据库 daemon 或 kernel identity 副本。

## 运行验收事实

- guest self-checking script 验证 64-bit integer `test`、外部 `[`/`[[`、环境读取以及有界 `yes` pipeline。
- scheduler 的唯一 Ready delivery seam 在目标 hart 已有 running task 时发布 reschedule，保证 syscall 密集型 pipe writer 不会饿死已唤醒 reader。
- 8 个并发 `mktemp` 创建得到 8 个不同的 `0600` 文件；directory template 为 `0700`，共享 `/tmp` 保持 `01777`。
- `install -D/-d/-p/-o/-g/-m` 创建 staged tree，保留 mtime 并提交 mode/UID/GID；缺失 source 不留下 destination。
- `stat` 验证 regular/symlink follow 边界、inode identity、size/time/owner/mode 与 filesystem block count。
- MD5、SHA-1、SHA-256、SHA-512 manifest 均由对应 `-c` 路径重新读取验证；`cksum` 独立核对 payload length。
- `whoami/groups` 通过 musl 标准 passwd/group lookup 返回 root。

## Gate 边界

Phase 55/56 的语义判断完全在各自 disposable image 内的 fail-fast shell script 完成；BusyBox init 以唯一 `sysinit` action 直接启动脚本，不经过 askfirst shell 或 UART 命令注入。host Python 只安装 runtime fixture、启动 QEMU、设置 deadline、拒绝 kernel error 并等待最终 marker。掉电、跨冷启动与交互 job-control 仍由 host orchestration 负责。`tty` 及其依赖的 opened-file identity 在 Phase 57 由独立竖切开放。
