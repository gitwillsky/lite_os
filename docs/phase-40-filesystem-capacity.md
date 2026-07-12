# Phase 40：文件系统容量可观测性竖切

## 目标

以 VFS 唯一 mount association 和 filesystem-owned 容量快照实现 Linux `statfs(43)/fstatfs(44)`，发布 `/proc/mounts`，并让固定动态 BusyBox `df` 成为真实 consumer。禁止 syscall 识别 ext2、按 filesystem id 复制统计、解析 procfs 回填 ABI，或给伪文件系统伪造可分配容量。

## 固定规范

- Linux `v7.1` commit `8cd9520d35a6c38db6567e97dd93b1f11f185dc6`：`include/uapi/asm-generic/statfs.h`、`include/uapi/asm-generic/unistd.h`、`fs/statfs.c`、`fs/ext2/super.c::ext2_statfs` 与 `fs/libfs.c::simple_statfs`。
- musl `v1.2.6` commit `9fa28ece75d8a2191de7c5bb53bed224c5947417`：generic `bits/statfs.h` 与 `src/stat/statvfs.c`。
- BusyBox `1.37.0`：`coreutils/df.c` 与 `libbb/find_mount_point.c`；默认 mount source 为 `/proc/mounts`，容量调用链为 `statvfs→statfs`。

RV64 `struct statfs` 固定为 120 bytes：七个 64-bit word 位于 0..56，两个 u32 fsid 位于 56/60，namelen/frsize/flags 位于 64/72/80，四个 spare word 位于 88..120 且必须清零。asm-generic syscall number 为 43/44。

## 唯一状态路径

1. VFS 唯一保存 root/boot mount 的 source、mountpoint、filesystem adapter 关联；pathname 与 inode-backed OFD 都由该关联选择 statistics owner。
2. ext2 在 allocator mutation lock 内复制 superblock 计数，按 Linux ext2 规则扣除 primary/backup superblock、GDT、bitmap 与 inode-table overhead，并将 UUID 折叠为 fsid。
3. procfs/devfs/anonymous pipe 不拥有 block allocator，按 Linux simple-statfs 返回零容量；procfs 使用 `PROC_SUPER_MAGIC`，pipe 使用 `PIPEFS_MAGIC`，devfs 以 RAMFS magic 表达其固定内存 inode tree。
4. syscall 层只编码 120-byte Linux UAPI、执行 user-copy 与 errno translation；`/proc/mounts` 直接投影 VFS mount table，不反向构造状态。

## 验收契约

- `/proc/mounts` 精确包含 root ext2、只读 devfs 与只读 procfs。
- BusyBox `df` 无参数可枚举 mount table，`df -P /` 返回非零 ext2 容量，`df -Pi /` 返回非零 inode 总数。
- 写入两个 4096-byte block 后 `df` available 下降，删除后由 ext2 唯一 allocator 回收。
- 动态 musl probe 同时验证 pathname statfs、root fd fstatfs 与 anonymous pipe `PIPEFS_MAGIC`。
- `make verify` 完整通过。
