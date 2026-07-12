# Phase 45：共享文件映射与唯一 page cache

Phase 45 以 Linux v7.1、POSIX.1-2024、musl 1.2.6 与 BusyBox 1.37.0 为固定语义源，完成 regular-file `MAP_SHARED` 竖切。范围不伪造 anonymous shared memory；该能力仍由 Phase 9 单独拥有。

## Owner 与依赖

- `fs::page_cache` 唯一持有 regular-file cache page、dirty 与 writable-PTE 计数；VFS read/write、ELF source、mmap fault、truncate 和 sync 不保留 storage 直通入口。
- ext2 `Inode` 只暴露 storage adapter，由 page cache 决定 coherence 与 writeback；ext2 journal 继续唯一拥有 metadata/data 提交顺序。
- memory 只提供共享物理页、VMA resident set 与 weak address-space invalidation registry，不依赖 inode 或 ext2；依赖保持 `fs -> memory` 单向。
- `AddressSpace` 是 PTE/VMA 生命周期 owner。fork 对 shared VMA 保持同页映射，private VMA 继续 COW；exec/drop 释放 resident writer membership。

## ABI 与生命周期

- `mmap(222)` 支持 lazy regular-file `MAP_SHARED`；writable shared mapping 要求可写 OFD，页内 EOF 尾部补零，整页起点越过 EOF 的 fault 交付 SIGBUS。
- read/write/pread/pwrite 与 mapped load/store 观察同一 cache page；truncate 清除尾部并跨地址空间撤销 EOF 外 PTE。
- `msync(227)` 固定采用 Linux flag validation；`MS_ASYNC` 不启动 I/O，`MS_SYNC` 同步 shared file range。munmap、fsync、sync 与 process exit 闭合 dirty writeback。
- clean boot 后的 shared mutation 经 `msync`、冷启动读取验证；power-cut gate 在持续 mapped mutation/writeback 中断电，随后 journal recovery 与只读 `e2fsck -fn` 必须通过。

## 明确剩余边界

物理页压力下只同步回收无外部引用的 clean cache page；dirty page 必须先走既有 writeback。anonymous `MAP_SHARED`、tmpfs/shmem、后台 writeback/reclaim worker 与 swap 不在本阶段，不得通过 page-cache 特判或私有共享 handle 提前出现。
