# Phase 43：链接、并发 namespace 与掉电一致性

## 目标

把 `symlinkat(36)`、`linkat(37)`、BusyBox `ln`、并发 `mkdir/rm/cp/mv/ln` 和 ext metadata crash consistency 收敛到同一个 filesystem mutation module。禁止 VFS link-count 状态、syscall 特判、仅靠写序的伪原子性或第二套非标准日志格式。

## 固定规范

- Linux `v7.1` fixed commit：`fs/namei.c`、`include/uapi/asm-generic/unistd.h`、`include/uapi/linux/fcntl.h`。
- 同 revision JBD2：`Documentation/filesystems/ext4/journal.rst`、`include/linux/jbd2.h`、`fs/jbd2/commit.c` 与 `recovery.c`。
- musl `v1.2.6` link/symlink/access wrappers；BusyBox `1.37.0` `coreutils/ln.c` 与 `libbb/remove_file.c`。

## 唯一 mutation 路径

VFS 只解析 old/new pathname、final-symlink policy 与 mount identity；ext2 adapter 在单一 mutation mutex 内拥有 directory entry、inode/link count、allocator、JBD2 transaction 与 `s_last_orphan` chain。transaction 把去重后的完整 block write-set 写入内置 journal inode，FLUSH 后发布 commit block，再 checkpoint home blocks并清空 journal；mount 先 replay commit，再回收 orphan，最后执行 consistency scan。

## 验收

动态 C consumer 直接验证 `linkat` 的 no-follow、`AT_SYMLINK_FOLLOW`、`AT_EMPTY_PATH`、inode identity 与 link count。BusyBox gate 验证 hard/soft link 生命周期，并让八个进程在同一父目录并发执行四轮 `mkdir/cp/ln/mv/rm`。掉电 gate 对同一镜像执行一次 open-unlink orphan 场景和四次不同延迟的 QEMU SIGKILL；最终 LiteOS 冷启动恢复并继续 mutation，宿主只读 `e2fsck` 必须完整通过。
