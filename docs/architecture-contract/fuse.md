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
  VFS 只在实际父目录锁内等待 LOOKUP/目录变更；全局 topology gate 不跨 FUSE 请求、页缓存同步或 remount。
  等待中的请求必须允许守护进程访问不相关目录并投影路径；目录与 mount 锁序由[存储契约](filesystem-storage.md)拥有。

本域引入的 VFS 接口与调用者：

- `InodeOpen::Storage(Option<Arc<dyn StorageOpen>>)`：OFD 的 `_storage_open` 持有 per-open 状态直到最后
  引用；FUSE 句柄是唯一非空调用者。`inode::open_by_type` 是 `open_backend` 的缺省实现，FUSE 对
  FIFO/设备节点复用它。
- `fs/storage_open` 拥有 `StorageOpen` 与 `DirectIo`，`Handle` 为唯一实际 backend；
  `StorageOpen::direct_io(Arc<Self>)` 投影 FOPEN_DIRECT_IO，`OpenFileDescription::regular_file` 将它传给
  `RegularFile::from_open`。所有用户 fd 读写（含 vector、positioned、sendfile/copy）由该入口进入；
  不把 per-open 标志放进 inode，也不从 inode 的任意句柄读写 direct 数据。
  直接读不按 i_size 截短、不填页、不补零，短读不收缩 EOF；直接写不使用 WRITE_CACHE，即使 INIT
  协商 WRITEBACK_CACHE。READ/WRITE 编解码与缓存路径复用，后续分块失败保留成功前缀并更新实际大小。
  append 在 user-copy/limit 检查前刷新 i_size，整次 syscall 沿用同一 write_sequence。
- direct 视图仍引用同一 `CachedFile`，不建立内容缓存或新锁。直接读在 operation 内写回范围内脏页再发 READ；
  直接写在 write_sequence 内、operation 外经 `memory::invalidate_cached_file` 撤销范围内共享与未 COW
  private cache aliases，复用 generation-aware MM registry、预分配 waiter 与远端 fence，保留独立 COW 数据。
  随后在 operation/epoch 内写回脏页、检查并发 fault/reader pin，再逐出范围页并发 WRITE；pin 仍存在返回 EBUSY，
  不能提交后再报告整次失败。异步旧填页由 epoch 拒绝，未缓存/无 alias 时不扫描 MM，缓存路径不经过这些步骤。
  io_uring 的直接读不走 cache nowait，而由既有 worker 执行；没有另加 FUSE 私有异步 ABI。
- `StorageOpen::prepare_mapping(shared)` 由 mmap 在 MAP_FIXED 替换前调用。未协商 DIRECT_IO_ALLOW_MMAP，
  direct MAP_SHARED（含只读）返回 ENODEV；MAP_PRIVATE 经现有 cache fault/COW 路径，打开与映射均可作废旧缓存。
  Benchmark 决策：复用既有锁/epoch，无新全局状态；direct 不分配内容页，失效只在相关 cache 页有 pin 时遍历 MM。
  waiter/请求分配与混合 mmap 的 fence 属新增成本；按用户要求不运行 benchmark，不调整基线或阈值。
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
- xattr 的 GET/SET/LIST/REMOVE 转发标准请求；未协商 SETXATTR_EXT，SET 使用 8-byte 兼容头。
  属性值和名称列表仅属于服务端，不建立本地 xattr 缓存；成功变更使 inode 属性缓存过期以重取 ctime。
  `Inode::xattr_size`/`xattr_list_size` 只由 OpenedFile 的零缓冲 syscall 路径使用，FUSE 发 size=0 查询；
  本地 filesystem 缺省沿用实际属性快照，security.capability 按 caller namespace 投影后计长。
  非零读取沿用 owned snapshot 接口，以 64 KiB 上限单次获取；小 caller 缓冲由统一 ABI 返回 ERANGE，
  最大缓冲仍装不下为 E2BIG，零缓冲查询按 Linux 将服务端长度截到上限。LIST 拒绝空名称/缺少 NUL 终止。
  四种操作分别缓存 ENOSYS→EOPNOTSUPP；ENODATA 在 raw inode 层为 NotFound，exec 因而正确识别无 capability。
  namespace、sticky、trusted/security capability 与 rootid 映射由共同 xattr policy 拥有；普通访问经
  `Inode::require_access`，FUSE 未启用 default_permissions 时由服务端判定。FUSE LIST 原样返回服务端名称，
  不套用本地 filesystem 的 trusted 名称过滤；POSIX ACL 的 system.* handler 仍未实现。
  Benchmark 决策：没有新增缓存/锁，零缓冲只收 8-byte 回复，普通读不先查询长度而多一次往返；
  本地属性的既有快照路径不变，新增 delegation 仅出现在 xattr 冷路径，沿用现有基线，未改阈值。

性能：请求按单页最多 256 页（1 MiB）分块；名字与属性缓存使每个路径分量在超时内不再往返。新增连接、
队列与 codec 都在 FUSE 自己的请求路径上，不进入既有文件系统热路径；`superblock()` 让 mount table 去掉
一次 statfs 调用。请求往返成本由阶段 6 的 Samba 运行时门禁在真实网络负载下测量。
