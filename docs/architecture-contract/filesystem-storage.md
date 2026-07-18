# 文件系统与存储契约

## Owner

- VFS namespace/inode 拥有 pathname identity；OpenFileDescription 拥有 backend、offset、status flag 与 descriptor reference consequence。
- FileDescriptorTable 独占 slot、FD_CLOEXEC、reservation、publication 与 lowest-free index。
- ext2 owner 独占 inode/directory/link/allocation mutation；JBD2 journal 独占 transaction/commit/replay；page cache 独占 cached page lifecycle。

## Interface

- filesystem 只通过 block seam 使用 driver，通过 shared-page seam 使用 memory，通过 unified backend façade 接入 pipe/socket/device。
- fd reservation 在 lookup/procfs/fork/close 前不可见；只有用户可见 fd number copyout 成功后才能 publish。
- pathname-backed OFD 必须保留 opened-entry identity；rename/unlink 不能把打开对象退化为字符串路径。
- packed disk layout、journal block、device adapter 与 syscall UAPI 不得穿过 VFS seam。

## Failure and cleanup

- rename/link/unlink/truncate 等 mutation 必须预留 journal/owner storage并提供完整 rollback；不能留下未索引 inode、错误 link count 或半提交 directory entry。
- close/dup/CLOEXEC 在 fd-table lock 内只 detach；OFD drop、epoll/flock/record-lock consequence 在锁外执行。
