# 文件系统与存储契约

## Owner

- VFS namespace/inode 拥有 pathname identity；OpenFileDescription 拥有 backend、file position、status flag 与 descriptor reference consequence。
- `FilePosition` 是 OFD position 的唯一 lock owner：sequential read/write、`lseek` 与 `getdents64`
  必须在单次 `with_position` 临界区内完成依赖 position 的完整操作；`sendfile` 必须通过
  `with_positions` 的稳定地址全序取得两个不同 OFD，禁止 syscall 直接取得 raw position lock。
- FileDescriptorTable 独占 slot、FD_CLOEXEC、reservation、publication 与 lowest-free index；fd slot 使用
  heap-owned 7/7/6-bit sparse radix，fullness summary 只能由同一 occupancy transition 更新。lookup、
  replace 与 take 固定三层，iteration/fork 只访问 materialized branch/chunk，禁止按最高 fd 物化或复制
  dense `Option<FileDescriptor>` table。RV64 reviewed payload 为 table inline 24 B、root/branch 各
  1040 B、64-slot FileDescriptor chunk 1024 B；仅 fd 0 与 1,048,575 时 heap payload 为 5168 B，
  全物化时 metadata/chunk payload 上限分别为 134,160 B/16,777,216 B（不含 allocator header）。
- ext2 owner 独占 inode/directory/link/allocation mutation；packed disk value 定义与字段保持
  `fs::ext2` parent-private，`fs::ext2::layout` 只封装定长 decode/encode 与 raw byte access，
  `fs::ext2::block_io` 封装 filesystem/device block 换算，`fs::ext2::inode` 独占 inode identity、
  block mapping 与 VFS projection；JBD2 journal 独占 transaction/commit/replay；page cache 独占 cached page lifecycle。
- `RegularFileWrite` 的 write-sequence 与 operation gates 共同独占一次 syscall 的 position、append placement、storage transaction 和 resident-cache publication 顺序。

## Interface

- filesystem 只通过 block seam 使用 driver，通过 shared-page seam 使用 memory，通过 unified backend façade 接入 pipe/socket/device。
- fd reservation 在 lookup/procfs/fork/close 前不可见；`recvmsg` 的 fd number 与全部关联 metadata
  copyout 成功后才能整批 publish，任一失败必须在 fd-table lock 外完成全部 reservation cleanup。
- OFD position 的推进只在对应 operation 已产生进度后发生；copyout 失败不得发布 `getdents64`
  position。`lseek` 结果必须能由 Linux signed `loff_t` 表示，不能把超出 `i64::MAX` 的值
  截断为负 syscall return。
- pathname-backed OFD 必须保留 opened-entry identity；rename/unlink 不能把打开对象退化为字符串路径。
- packed disk layout、journal block、device adapter 与 syscall UAPI 不得穿过 VFS seam。
- regular write 以 32 logical pages/128 KiB 为最大 transient batch，并复用 page-cache writeback 的 capacity backoff；非对齐 128 KiB 可触及 33 个 filesystem pages，必须由实际 journal `NoSpace` 退避，禁止假定固定物理页数。
- 小于等于 4 KiB 的 regular write 使用 stack staging；大请求 heap reserve、zero-fill 与最终 deallocation 必须位于 OFD position/write-sequence gate 外，失败时退回 4 KiB stack progress，不得新增 `ENOMEM`。heap staging 不得超过 128 KiB，且不形成 persistent state。

## Failure and cleanup

- rename/link/unlink/truncate 等 mutation 必须预留 journal/owner storage并提供完整 rollback；不能留下未索引 inode、错误 link count 或半提交 directory entry。
- close/dup/CLOEXEC 在 fd-table lock 内只 detach；OFD drop、epoll/flock/record-lock consequence 在锁外执行。
- regular gather 必须按 user-page 边界 copy，使单个跨有效/坏页 iovec 仍可提交坏页前 prefix；backend short/error 后只推进 durable prefix。RLIMIT_FSIZE 在 non-append copyin 前裁剪，append 在 operation lock 内按 inode end 裁剪并保持 SIGXFSZ/EFBIG 与 position 语义。
- regular batching 的 blocking metric 使用 deterministic backend counters：对齐 1 MiB sequential write 从 256 个 journal transactions/1024 次 flush 收敛为 8/32；33-page 非对齐形状必须证明 capacity failure 无 publication 且退避后连续提交。wall time 仅作诊断，不作为 host gate。
