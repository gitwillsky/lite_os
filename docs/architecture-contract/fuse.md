# FUSE 契约

本文件是 [文件系统与存储契约](filesystem-storage.md) 的 FUSE 部分。

`fs/fuse` 实现 Linux FUSE（协议 7.45）的 `/dev/fuse` 与 `fuse` 类型；守护进程的回复是唯一事实来源，内核只持有
带超时的缓存。

- **owner**：`Connection` 独占请求队列（pending/interrupts/forgets/processing）、INIT 协商结果、请求 unique
  计数与绑定设备数；`Mount` 独占 nodeid → `Weak<FuseInode>` 索引（`Mount.nodes`）与守护进程以 ENOSYS 声明
  不实现的可选操作位；`FuseInode` 独占查找计数、属性缓存、名字缓存（`FuseInode.entries`，强持子 inode 到
  entry 超时）与本 inode 打开句柄的 Weak 列表；`Handle` 是 OFD 的 `StorageOpen`，Drop 发 RELEASE。
  `/dev/fuse` 的 `FuseDevice` 只持连接绑定与设备标识。连接持 `Weak<Mount>` 处理通知，不形成环。
- **请求生命周期**：INIT 在挂载时入队，其余请求在一次性 latch 上等 INIT；守护进程读取后请求移入
  读取设备的 processing，回复只接受同一设备写回。等待依次为可中断（被打断即标记并补发 INTERRUPT）、
  仅 SIGKILL（仍在队列中则撤回返回 EINTR）、不可中断。最后一个设备关闭或卸载中止连接：未完成请求以
  ENOTCONN（协商 ABORT_ERROR 时 ECONNABORTED）结束，守护进程 read 得 ENODEV。
- **不能失败的路径**：RELEASE 在 OPEN 前构造并占用 pending 预留槽，FORGET 在查找前预留 forgets 槽；Drop
  只消费预留，不分配。READDIRPLUS 按一页回复的最大项数预留，未用部分退还；守护进程为每个非 `.`/`..`
  项计了一次查找，即使 visitor 中途停止也全部接管。查找回复建 inode 失败时立即 FORGET 该次查找。
- **数据与页缓存**：普通文件经全局 page cache（mmap/exec 可用）；未协商 WRITEBACK_CACHE 时 write 为
  write-through，协商后写只进页缓存（`Inode::buffered_writes`），大小与 mtime/ctime 以内核为准，只写打开
  对服务器升级为读写，close 在 FLUSH 前写回，最后一个可写句柄在 RELEASE 前用自己的 fh 写出剩余脏页
  （`draining`，同样保护 write-through 模式下的 mmap 脏页）；未带
  FOPEN_KEEP_CACHE 的 OPEN 与守护进程 INVAL_INODE 调用 `page_cache::invalidate`，写回脏页并丢弃只被缓存
  持有的干净页，映射中的页保留到解除。属性刷新发现大小变化、或 AUTO_INVAL_DATA 下 mtime 变化时标记
  `stale_data`，读入口与打开时完成作废。短读把缓存大小收缩到实际 EOF 并补零。
- **权限**：未 `allow_other` 时只允许挂载用户；`default_permissions` 按缓存属性做内核 DAC；否则
  access(2)/chdir/chroot 发 FUSE_ACCESS，执行普通文件要求执行位，其余由服务器判定，chmod/chown 以
  `OwnerModeChange::requested` 原样交给服务器。
- **身份**：`st_ino` 取 nodeid（VFS 以 inode 号区分同一 filesystem 内的 inode），`d_ino` 取守护进程报告值；
  两者一致需要守护进程令 `ino == nodeid`（libfuse 未设 use_ino 时即如此）。
- **锁序**：`Connection.queue` → `Request.phase`；inode 的 attributes/entries/handles 各自独立，互不嵌套；
  名字缓存与索引的被替换项在锁外析构，子 inode 的最终 Drop 可重入连接。目录操作在守护进程未声明
  PARALLEL_DIROPS 时取 `directory_operations`，持有期间只发请求，不取其他 inode 的锁。

本域引入的 VFS 接口与调用者：

- `InodeOpen::Storage(Option<Arc<dyn StorageOpen>>)`：OFD 的 `_storage_open` 持有 per-open 状态直到最后
  引用；FUSE 句柄是唯一非空调用者。`inode::open_by_type` 是 `open_backend` 的缺省实现，FUSE 对
  FIFO/设备节点复用它。
- `StorageOpen::flush(owner)` 与 `OpenFileDescription::flush`：任务层 `close_detached` 是 Linux `filp_close`
  的唯一实现（flush → descriptor cleanup → 本表 POSIX 锁释放），close/dup2 替换/CLOEXEC/ADDFD 替换共用；
  `FileTableOwner` 最后 Drop 对剩余 fd 逐个 flush（Linux `close_files`）。只有 `close(2)` 返回 flush 错误。
  FUSE 句柄发 FUSE_FLUSH（目录与 FOPEN_NOFLUSH 除外，ENOSYS 记为 `no_flush`）。
- `Inode::create_open()`（Linux `atomic_open`）：`open(O_CREAT)` 创建普通文件时由 VFS `create_child` 调用，
  返回的 backend 直接用于 OFD，不再 `open_backend`；缺省 `None` 走 create + open。FUSE 发 CREATE，回复前预留
  FORGET 与 RELEASE（nodeid 回复后写入），ENOSYS 记为 `no_create` 并回退 MKNOD + OPEN。
- `page_cache::write_back(SharedFileId)`：不建立持久化边界的写回；FUSE close（FLUSH 前）与最后可写句柄释放是
  唯一调用者。
- `MountRequest.files: &dyn CallerFiles`（syscall `TaskFiles`，`fd=` 经它 `fget`，不接受 O_PATH）与
  `MountEnvironment.requester`（`task::current_requester`，请求头 uid/gid/pid 与挂载 PID namespace）。
- `FileSystemType::has_subtype` / `get_filesystem_type` 返回子类型：`fuse.<x>` 由 syscall 以 `subtype=`
  先于 data 交给类型，空子类型 EINVAL；`FileSystem::subtype` 供 mount table 显示 `<type>.<subtype>`。
- `FileSystem::superblock() -> SuperblockFacts`：类型名与 SB 只读，mount table 与 superblock 复用只读它，
  `statistics()` 只服务 statfs，VFS 在其中补 SB 只读位；守护进程卡住不会阻塞 `/proc/mounts`。
- `MAY_ACCESS`/`MAY_CHDIR`：`require_access` 掩码的意图位，DAC 忽略，由 access/chdir/fchdir/chroot 传入。
- `FileSystemError::Errno(u16)`：后端直接给出的 errno（守护进程回复、ENOTCONN、EINTR）；VFS 依赖语义的
  errno 仍用对应变体。
- `page_cache::invalidate(SharedFileId, wait)`：FUSE 是唯一调用者。打开与失效通知等待 write/truncate；
  属性刷新与读入口只 `try_lock`，忙则由 inode 的 `stale_data` 保留需求，避免同一线程持写序列时自锁。
- `Inode::revalidate_data()`：`RegularFile::read` 在取任何页缓存锁前调用；缺省空，FUSE 在 AUTO_INVAL_DATA 下刷新
  过期属性并完成未决作废（Linux `fuse_file_read_iter` → `fuse_update_attributes`）。
- `Inode::identity()`：不发起 I/O 的 `(st_dev, st_ino)`，dentry 索引、mountinfo、rename 同一性、fdinfo 与
  page-cache 缺省身份只读它；缺省取 metadata，FUSE 覆盖为 nodeid，路径解析不再为每个分量 GETATTR。
- `Inode::buffered_writes()`/`extend_buffered_size()`：页缓存缓冲写；覆盖整页或 EOF 之后的页直接清零，EOF 内
  部分写先填充，脏页由 `mark_dirty` 计入全局 dirty，全局达到高水位时写者同步写回本文件。缺省 false，
  ext4/tmpfs 的 write-through 不变。`writeback_range(.., durable)`：fsync/msync/sync/umount 为 true（调用
  `sync_storage`），缓存作废与 `page_cache::write_back`（FUSE close/句柄释放）为 false，不额外发 FSYNC。
- `Inode::readahead_pages()`/`read_storage_pages()`：页缓存未命中一次填充从缺失页起、不越过 EOF 与下一个已缓存页的
  连续页（栈上最多 32 页，miss 路径不分配堆）。缺省窗口 1 页、逐页读取，ext4/tmpfs 行为不变；FUSE 返回 INIT 协商
  的 `max_readahead`（≤128 KiB 且 ≤ 单请求页数），整窗一个 READ。预读页准备失败只缩短窗口。
- `Inode::unlinked(directory)`：VFS 在 unlink/rmdir 与 rename 覆盖成功后通知 victim（Linux `drop_nlink`）；
  ext4/tmpfs 在父目录事务中维护链接数，保持空实现；FUSE 据此把缓存链接数降到零，页缓存随最后 dentry 逐出。

性能：请求按单页最多 256 页（1 MiB）分块；名字与属性缓存使每个路径分量在超时内不再往返。新增连接、
队列与 codec 都在 FUSE 自己的请求路径上，不进入既有文件系统热路径；`superblock()` 让 mount table 去掉
一次 statfs 调用。请求往返成本由阶段 6 的 Samba 运行时门禁在真实网络负载下测量。
