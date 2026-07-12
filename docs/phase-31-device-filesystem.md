# Phase 31：boot-time device filesystem

## 目标与分层

rootfs builder 只在 ext2 中预建空 `/dev` mountpoint，不写入伪设备文件。kernel composition root 在 ext2 mount 后把内存 device filesystem 作为第二个 `FileSystem` adapter 挂载到 `/dev`。mount enter/leave、`..`、cwd 反向解析和跨 filesystem rename 判定都由 VFS 唯一 mount table 拥有；syscall 不检查 `/dev` 字符串。

device filesystem 是唯一 singleton owner，使用稳定 `st_dev=2`；VFS 同时拒绝同一 filesystem root 重复挂载。它固定公布 Linux conventional character nodes：`null(1,3)`、`zero(1,5)`、`tty(5,0)` 和 `console(5,1)`。inode metadata/getdents 报告 character type 与 `st_rdev`；open 后全部进入唯一 `OpenFileKind::Character`。`tty/console` 持有 Process 继承的同一 Terminal owner，`tty` 额外校验 caller session 的 controlling TTY；`null/zero` 实现标准 byte-stream 语义。

## 验收

BusyBox gate 通过真实 ash 完成：

1. `ls /dev` 枚举四个 character node；
2. redirect 到 `/dev/null`；
3. `dd` 从 `/dev/zero` 读回四个 zero byte；
4. 重新打开 `/dev/tty` 与 `/dev/console` 输出 marker；
5. `chdir /dev` 后 `getcwd` 返回 `/dev`；
6. 同一默认 rootfs 继续通过 1/3/8-hart 启动、pipeline、Ctrl-C 与持久化 gate。

## 明确边界

当前 device filesystem 是固定节点的最小不可变 adapter，mutation 返回 `EROFS`；它不声称 Linux devtmpfs 的动态 device-model/hotplug/mknod 语义。ext2 中的未注册 special inode 也会被拒绝，不得被当作 regular file 绕过 device owner。该范围缩减是显式的，不通过 regular-file fallback 或伪成功 syscall 补齐。
