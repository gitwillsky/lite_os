# LiteOS Architecture Contract

本文只维护全局 module dependency、机器读取的精确 registry、source-size review 与跨领域变更规则。领域 owner、interface 和 cleanup 约束由子契约唯一维护。

## 领域契约

| 领域 | 权威契约 |
|---|---|
| 启动与平台 | [boot-platform](architecture-contract/boot-platform.md) |
| 执行 | [execution](architecture-contract/execution.md) |
| 内存 | [memory](architecture-contract/memory.md) |
| 进程与调度 | [process-scheduling](architecture-contract/process-scheduling.md) |
| 文件系统与存储 | [filesystem-storage](architecture-contract/filesystem-storage.md) |
| IPC 与网络 | [ipc-network](architecture-contract/ipc-network.md) |
| 设备与终端 | [devices-terminal](architecture-contract/devices-terminal.md) |
| 音频 | [audio](architecture-contract/audio.md) |
| 异步 I/O | [io-uring](architecture-contract/io-uring.md) |
| 图形会话与 LiteUI | [lite-runtime](architecture-contract/lite-runtime.md) |
| 用户态与 ABI | [userspace-abi](architecture-contract/userspace-abi.md) |

## 2. Kernel dependency contract

领域依赖理由见 [依赖说明](architecture-contract/dependency-owners.md)。

| Module | 允许依赖（机器读取） | 说明 |
|---|---|---|
| `arch` | `config`,`fallible_tree` | 领域规则见依赖说明 |
| `entry` | `cpu`,`platform`,`trap` | 领域规则见依赖说明 |
| `config` | 无 | 领域规则见依赖说明 |
| `cpu` | `arch` | 领域规则见依赖说明 |
| `hal` | `arch`,`sync` | 领域规则见依赖说明 |
| `deferred` | `arch`,`cpu` | 领域规则见依赖说明 |
| `platform` | `arch`,`cpu`,`fallible_tree`,`hal`,`sync` | 领域规则见依赖说明 |
| `fallible_tree` | 无 | 领域规则见依赖说明 |
| `sync` | `arch`,`cpu` | 领域规则见依赖说明 |
| `memory` | `arch`,`config`,`cpu`,`fallible_tree`,`id`,`platform`,`sync` | 领域规则见依赖说明 |
| `drivers` | `deferred`,`fallible_tree`,`hal`,`memory`,`sync` | 领域规则见依赖说明 |
| `block` | `hal` | 领域规则见依赖说明 |
| `boot_screen` | `drivers` | 领域规则见依赖说明 |
| `virtio` | `block`,`deferred`,`drivers`,`hal`,`memory`,`platform` | 领域规则见依赖说明 |
| `virtio_port` | `deferred`,`drivers`,`fs`,`ipc` | 领域规则见依赖说明 |
| `audio` | `drivers`,`fs`,`id`,`ipc`,`memory` | 领域规则见依赖说明 |
| `drm` | `deferred`,`drivers`,`fallible_tree`,`fs`,`ipc`,`memory`,`socket`,`sync` | 领域规则见依赖说明 |
| `input` | `deferred`,`drivers`,`fs`,`ipc`,`timer` | 领域规则见依赖说明 |
| `io_uring` | `fs`,`id`,`ipc`,`memory`,`namespace`,`sync`,`timer` | 领域规则见依赖说明 |
| `ipc` | `fallible_tree`,`id`,`memory`,`namespace`,`sync` | 领域规则见依赖说明 |
| `socket` | `drivers`,`fallible_tree`,`id`,`ipc`,`namespace`,`sync`,`timer` | 领域规则见依赖说明 |
| `fs` | `block`,`deferred`,`fallible_tree`,`id`,`ipc`,`log`,`memory`,`namespace`,`random`,`socket`,`sync`,`timer` | 领域规则见依赖说明 |
| `file` | `fallible_tree`,`fs`,`ipc`,`memory`,`namespace`,`socket`,`sync` | 领域规则见依赖说明 |
| `tty` | `fallible_tree`,`fs`,`hal`,`ipc`,`namespace`,`sync`,`timer` | 领域规则见依赖说明 |
| `namespace` | `fallible_tree` | 领域规则见依赖说明 |
| `task` | `arch`,`cpu`,`deferred`,`drivers`,`fallible_tree`,`file`,`fs`,`id`,`ipc`,`memory`,`namespace`,`platform`,`random`,`socket`,`sync`,`timer`,`tty` | 领域规则见依赖说明 |
| `trap` | `arch`,`cpu`,`deferred`,`hal`,`memory`,`platform`,`syscall`,`task`,`timer` | 领域规则见依赖说明 |
| `syscall` | `file`,`fs`,`io_uring`,`ipc`,`memory`,`namespace`,`random`,`socket`,`sync`,`system`,`task`,`timer` | 领域规则见依赖说明 |
| `random` | `drivers` | 领域规则见依赖说明 |
| `system` | `arch`,`cpu`,`platform` | 领域规则见依赖说明 |
| `timer` | `arch`,`cpu`,`drivers`,`platform`,`sync` | 领域规则见依赖说明 |
| `log` | `cpu`,`deferred`,`platform`,`sync`,`timer` | 领域规则见依赖说明 |
| `id` | 无 | 领域规则见依赖说明 |
| `cmdline` | 无 | 领域规则见依赖说明 |
| `lang_item` | `arch`,`cpu`,`platform` | 领域规则见依赖说明 |
| `main` | `arch`,`audio`,`block`,`boot_screen`,`cmdline`,`config`,`cpu`,`deferred`,`drivers`,`drm`,`entry`,`fallible_tree`,`file`,`fs`,`hal`,`id`,`input`,`ipc`,`log`,`memory`,`namespace`,`platform`,`random`,`socket`,`sync`,`syscall`,`system`,`task`,`timer`,`trap`,`tty`,`virtio_port`,`virtio` | 见依赖说明 |

同一 module 内引用不构成跨 seam 依赖。`main.rs` 可以依赖所有 kernel module，但只能做装配、启动顺序和 fail-stop 策略。正向 dependency matrix 必须无环；围栏检查 AST 与 macro token 路径（含 `$crate`、分组/别名 import），禁止 crate-root alias 绕过 owner。

## State-owner registry

AVL mechanism 与审查规则见 [有序存储与源码审查](architecture-contract/ordered-storage.md)。

### Persistent FallibleMap registry

下表是生产 kernel 中全部持久 `FallibleMap` 字段与 static 的精确集合。checker 对 location 与完整类型双向比对；新增、删除、换型或遗漏登记都必须先完成 owner/interface review。函数局部 transaction scratch 不属于持久 owner，不在此登记。

| Location | Type |
|---|---|
| `kernel/src/memory/shared_file.rs :: MappingRegistry.slots` | `FallibleMap < usize , Weak < dyn MemoryMappingOwner > >` |
| `kernel/src/memory/shared_file.rs :: ReclaimerRegistry.slots` | `FallibleMap < usize , Weak < dyn MemoryReclaimer > >` |
| `kernel/src/platform/qemu_virt/riscv64/plic.rs :: PlicInterruptController.affinities` | `FallibleMap < InterruptVector , CpuSet >` |
| `kernel/src/platform/qemu_virt/riscv64/plic.rs :: PlicInterruptController.handlers` | `FallibleMap < InterruptVector , Arc < dyn InterruptHandler > >` |
| `kernel/src/platform/qemu_virt/aarch64/gicv3.rs :: GicV3.handlers` | `FallibleMap < InterruptVector , Arc < dyn InterruptHandler > >` |
| `kernel/src/tty/pty.rs :: PtyRegistry.slots` | `FallibleMap < u32 , Weak < PtyPair > >` |
| `kernel/src/drm/mod.rs :: DrmDeviceState.graphics_cleanups` | `FallibleMap < u32 , graphics :: VirglCleanup >` |
| `kernel/src/drm/mod.rs :: DrmDeviceState.framebuffers` | `FallibleMap < u32 , Framebuffer >` |
| `kernel/src/drm/mod.rs :: DrmFileState.buffers` | `FallibleMap < u32 , Arc < DumbBuffer > >` |
| `kernel/src/drm/mod.rs :: DrmFileState.graphics_buffers` | `FallibleMap < u32 , Arc < graphics :: VirglBuffer > >` |
| `kernel/src/drm/graphics.rs :: VirglCleanup.buffers` | `FallibleMap < u32 , Arc < VirglBuffer > >` |
| `kernel/src/drm/publication_order.rs :: PublicationIdAllocator.reusable` | `FallibleMap < T , () >` |
| `kernel/src/drivers/io_completion/request_owner.rs :: RequestOwner.capacity_waiters` | `FallibleMap < u64 , Arc < CapacityWait > >` |
| `kernel/src/file/epoll.rs :: EpollState.interests` | `FallibleMap < InterestKey , Interest >` |
| `kernel/src/file/epoll.rs :: EpollState.ready` | `FallibleMap < InterestKey , () >` |
| `kernel/src/file/epoll.rs :: EpollMemberships.entries` | `Mutex < FallibleMap < ReverseKey , ReverseMembership > >` |
| `kernel/src/file/epoll.rs :: static SOURCE_INDEX` | `Mutex < FallibleMap < SourceIndexKey , SourceMembership > >` |
| `kernel/src/fs/inode_identity.rs :: LiveInodes[0]` | `Mutex < FallibleMap < K , Weak < T > > >` |
| `kernel/src/namespace/quota.rs :: QuotaState.users` | `FallibleMap < u32 , [u32 ; UserResource :: COUNT] >` |
| `kernel/src/fs/inotify.rs :: WatchIndex.by_inode` | `FallibleMap < (usize , u64) , InodeWatches >` |
| `kernel/src/fs/inotify.rs :: InodeWatches.watches` | `FallibleMap < WatchKey , Arc < Watch > >` |
| `kernel/src/fs/inotify/instance.rs :: WatchMembership.watches` | `FallibleMap < i32 , Arc < Watch > >` |
| `kernel/src/fs/inotify/instance.rs :: WatchMembership.identities` | `FallibleMap < (usize , u64) , i32 >` |
| `kernel/src/fs/inotify/queue.rs :: EventQueue.events` | `FallibleMap < u64 , Event >` |
| `kernel/src/fs/vfs/dentry_index.rs :: DentryIndex[0]` | `Mutex < FallibleMap < DentryKey , Weak < Dentry > > >` |
| `kernel/src/fs/vfs/mount_index.rs :: static ATTACHMENTS` | `Mutex < FallibleMap < AttachmentKey , Weak < MountView > > >` |
| `kernel/src/fs/vfs/mounted_filesystem.rs :: static FILESYSTEMS` | `Mutex < FallibleMap < usize , Weak < MountedFilesystem > > >` |
| `kernel/src/fs/ext4.rs :: Ext4FileSystem.inode_cache` | `Mutex < FallibleMap < u32 , Weak < Ext4Inode > > >` |
| `kernel/src/fs/ext4/journal/staged_blocks.rs :: StagedBlocks.entries` | `FallibleMap < u64 , StagedBlock >` |
| `kernel/src/fs/memory_file/sparse.rs :: SparsePages.pages` | `FallibleMap < u64 , Arc < P > >` |
| `kernel/src/fs/tmpfs.rs :: Namespace.inodes` | `FallibleMap < u64 , Weak < TmpInode > >` |
| `kernel/src/fs/xattr.rs :: Xattrs.entries` | `FallibleMap < Vec < u8 > , Vec < u8 > >` |
| `kernel/src/fs/tmpfs/directory.rs :: Directory.names` | `FallibleMap < Key , Slot < T > >` |
| `kernel/src/fs/tmpfs/directory.rs :: Directory.order` | `FallibleMap < u64 , Key >` |
| `kernel/src/fs/block_claim.rs :: DiskClaims.extents` | `spin :: Mutex < FallibleMap < u64 , Extent > >` |
| `kernel/src/fs/ext4/journal.rs :: ActiveTransaction.freed` | `FallibleMap < u64 , u64 >` |
| `kernel/src/fs/ext4/journal.rs :: RunningTransaction.freed` | `FallibleMap < u64 , u64 >` |
| `kernel/src/fs/page_cache/registry.rs :: static FILES` | `Once < Mutex < FallibleMap < SharedFileId , Arc < CachedFile > > > >` |
| `kernel/src/fs/page_cache/reclaim.rs :: CachedPages.entries` | `FallibleMap < u64 , Arc < CachedPage > >` |
| `kernel/src/arch/riscv64/page_table.rs :: PageTable.table_pages` | `FallibleMap < usize , Page >` |
| `kernel/src/arch/aarch64/page_table.rs :: PageTable.table_pages` | `FallibleMap < usize , Page >` |
| `kernel/src/memory/mm/area.rs :: MapArea.data_frames` | `FallibleMap < VirtualPageNumber , PrivateResident >` |
| `kernel/src/memory/swappable_page/registry.rs :: Registry.slots` | `FallibleMap < usize , Weak < SwappablePage > >` |
| `kernel/src/memory/mm.rs :: MemorySet.areas` | `FallibleMap < VirtualPageNumber , MapArea >` |
| `kernel/src/memory/mm/shared_area.rs :: AnonymousSharedPages.frames` | `FallibleMap < usize , Arc < SwappablePage > >` |
| `kernel/src/ipc/shared_memory_creator.rs :: SharedMemoryCreatorList.state` | `Mutex < Option < FallibleMap < CreatorKey , CreatorRegistration > > >` |
| `kernel/src/memory/mm/shared_area.rs :: SharedFileArea.resident` | `FallibleMap < VirtualPageNumber , SharedResident >` |
| `kernel/src/memory/mm/shootdown.rs :: TranslationCommit.retired_table_pages` | `FallibleMap < usize , FrameTracker >` |
| `kernel/src/socket/inet.rs :: NetworkStack.endpoints` | `FallibleMap < SocketHandle , EndpointState >` |
| `kernel/src/socket/inet.rs :: NetworkStack.raw_endpoints` | `FallibleMap < SocketHandle , raw :: RawEndpointState >` |
| `kernel/src/socket/inet.rs :: NetworkStack.tcp_endpoints` | `FallibleMap < usize , TcpEndpointState >` |
| `kernel/src/task/process_table/group_retirement.rs :: RetiredGroups.nodes` | `FallibleMap < usize , ProcessGroupIndex >` |
| `kernel/src/namespace/pid.rs :: State.numbers` | `FallibleMap < u32 , Option < PidAssociation > >` |
| `kernel/src/socket/inet/port_namespace.rs :: Occupancy.specific` | `FallibleMap < Ipv4Addr , AddressOccupancy >` |
| `kernel/src/socket/inet/port_namespace.rs :: PortNamespace.entries` | `FallibleMap < u16 , Occupancy >` |
| `kernel/src/socket/packet.rs :: PacketRegistry.endpoints` | `FallibleMap < usize , EndpointState >` |
| `kernel/src/socket/kobject.rs :: KobjectRegistry.endpoints` | `FallibleMap < u64 , Weak < KobjectSocket > >` |
| `kernel/src/socket/unix/namespace.rs :: static NAMESPACE` | `Once < Mutex < FallibleMap < NamespaceKey , Weak < UnixSocket > > > >` |
| `kernel/src/socket/unix/rights_graph.rs :: NodeState.edges` | `FallibleMap < EdgeKey , GraphEdge >` |
| `kernel/src/socket/unix/rights_graph.rs :: RetiredGraph.edges` | `FallibleMap < EdgeKey , GraphEdge >` |
| `kernel/src/socket/unix/rights_graph.rs :: RetiredGraph.nodes` | `FallibleMap < u64 , Arc < GraphNode > >` |
| `kernel/src/socket/unix/datagram_queue.rs :: DatagramQueue.entries` | `FallibleMap < u64 , T >` |
| `kernel/src/socket/unix/rights_graph.rs :: RightsGraph.nodes` | `FallibleMap < u64 , Arc < GraphNode > >` |
| `kernel/src/socket/unix/rights_graph.rs :: RightsGraph.uid_inflight` | `FallibleMap < u32 , usize >` |
| `kernel/src/socket/unix/connection_backlog.rs :: ConnectionBacklog.pending` | `FallibleMap < u64 , T >` |
| `kernel/src/task/process_table/process_retirement.rs :: RetiredProcesses.nodes` | `FallibleMap < usize , ProcessNode >` |
| `kernel/src/task/process_table/process_retirement.rs :: RetiredProcesses.parents` | `FallibleMap < usize , () >` |
| `kernel/src/task/process_table/process_retirement.rs :: RetiredProcesses.creators` | `FallibleMap < usize , () >` |
| `kernel/src/task/process_table/process_retirement.rs :: RetiredProcesses.groups` | `FallibleMap < usize , () >` |
| `kernel/src/task/process_table.rs :: ProcessGraph.nodes` | `FallibleMap < usize , ProcessNode >` |
| `kernel/src/task/process_table.rs :: ProcessGraph.groups` | `FallibleMap < usize , ProcessGroupIndex >` |
| `kernel/src/task/process_table.rs :: ProcessGraph.kernel_threads` | `FallibleMap < usize , Arc < TaskControlBlock > >` |
| `kernel/src/task/process_table.rs :: ProcessGraph.threads` | `FallibleMap < usize , ThreadIndex >` |
| `kernel/src/task/process_table.rs :: ProcessNode.children` | `FallibleMap < usize , () >` |
| `kernel/src/task/process_table.rs :: ProcessNode.child_waiters` | `FallibleMap < usize , Arc < TaskControlBlock > >` |
| `kernel/src/task/process_table.rs :: ProcessState::Live[0]` | `FallibleMap < usize , Arc < TaskControlBlock > >` |
| `kernel/src/task/process_table.rs :: ProcessGroupIndex.members` | `FallibleMap < usize , () >` |
| `kernel/src/task/process_table.rs :: ThreadIndex.created_children` | `FallibleMap < usize , () >` |
| `kernel/src/task/process_table/timer_queue.rs :: TimerQueue.deadline_index` | `FallibleMap < (u64 , TimerIdentity) , Option < Weak < KernelTimer > > >` |
| `kernel/src/task/process_table/timer_queue.rs :: TimerQueue.posix_timers` | `FallibleMap < (usize , i32) , PosixTimer >` |
| `kernel/src/task/process_table/timer_queue.rs :: TimerQueue.real_timers` | `FallibleMap < usize , RealTimer >` |
| `kernel/src/task/process_table/timer_queue.rs :: TimerQueue.timer_files` | `FallibleMap < u64 , TimerFile >` |
| `kernel/src/task/process_table/signal/job_control.rs :: JobNotification.waiters` | `FallibleMap < usize , Arc < TaskControlBlock > >` |
| `kernel/src/task/process_table/wait_registry/batch.rs :: ClaimedBatch.entries` | `FallibleMap < WaitIndexKey , Arc < WaitRegistration > >` |
| `kernel/src/task/process_table/wait_registry/shard.rs :: WaitShard.index` | `FallibleMap < WaitIndexKey , Arc < WaitRegistration > >` |

| `kernel/src/ipc/ids.rs :: IpcIds.keys` | `FallibleMap < i32 , u16 >` |
| `kernel/src/ipc/ids.rs :: IpcIds.objects` | `FallibleMap < u16 , Arc < T > >` |
| `kernel/src/ipc/message_queue.rs :: QueueState.messages` | `FallibleMap < u64 , Arc < Message > >` |
| `kernel/src/ipc/message_queue.rs :: QueueState.waiters` | `FallibleMap < u64 , Arc < QueueWaiter > >` |
| `kernel/src/ipc/message_queue.rs :: RetiredMessages.messages` | `FallibleMap < u64 , Arc < Message > >` |
| `kernel/src/ipc/message_queue.rs :: RetiredMessages.waiters` | `FallibleMap < u64 , Arc < QueueWaiter > >` |
| `kernel/src/ipc/sem_undo.rs :: UndoListState.entries` | `FallibleMap < UndoKey , Arc < SemaphoreUndoEntry > >` |
| `kernel/src/ipc/semaphore.rs :: RetiredSemaphores.undo` | `FallibleMap < usize , UndoRegistration >` |
| `kernel/src/ipc/semaphore.rs :: RetiredSemaphores.waiters` | `FallibleMap < u64 , Arc < PendingOperation > >` |
| `kernel/src/ipc/semaphore.rs :: SemaphoreState.undo` | `FallibleMap < usize , UndoRegistration >` |
| `kernel/src/ipc/semaphore.rs :: SemaphoreState.waiters` | `FallibleMap < u64 , Arc < PendingOperation > >` |
| `kernel/src/namespace/rlimit_counts.rs :: RlimitCounters.users` | `Mutex < FallibleMap < (RlimitResource , u32) , u64 > >` |
| `kernel/src/socket/inet/route.rs :: RoutingTable.prefixes` | `[FallibleMap < Key , Record > ; 33]` |
| `kernel/src/socket/inet/route_reply.rs :: RouteReplies.packets` | `FallibleMap < u64 , Reply >` |
| `kernel/src/socket/kobject.rs :: KobjectRegistry.ports` | `FallibleMap < (u8 , u64 , u32) , u64 >` |
| `kernel/src/socket/kobject.rs :: KobjectSocketState.queue` | `FallibleMap < u64 , Uevent >` |
| `kernel/src/socket/net_namespace.rs :: NetworkNamespaceDirectory.entries` | `crate :: fallible_tree :: FallibleMap < u64 , Weak < NetNamespace > >` |
| `kernel/src/fs/procfs/lookup_cache.rs :: ProcLookupCache[0]` | `Mutex < FallibleMap < (ProcNode , u64) , Arc < ProcInode > > >` |
| `kernel/src/fs/vfs/mount_unmount.rs :: Selection.index` | `FallibleMap < usize , usize >` |
| `kernel/src/task/process_table/timer_queue/retirement.rs :: RetiredTimers.deadlines` | `FallibleMap < (u64 , TimerIdentity) , Option < Weak < KernelTimer > > >` |
| `kernel/src/task/process_table/timer_queue/retirement.rs :: RetiredTimers.files` | `FallibleMap < u64 , TimerFile >` |
| `kernel/src/task/process_table/timer_queue/retirement.rs :: RetiredTimers.posix` | `FallibleMap < (usize , i32) , PosixTimer >` |
| `kernel/src/task/process_table/timer_queue/retirement.rs :: RetiredTimers.real` | `FallibleMap < usize , RealTimer >` |

## 4. Source size contract

| Source | Reviewed max lines | Owner | Reason | Exit criterion |
|---|---:|---|---|---|
| `kernel/src/fs/procfs.rs` | 1469 | fs | 同一 ProcNode 的完整投影 | 新增独立目录 family 前拆分 |
| `kernel/src/fs/tmpfs.rs` | 1020 | fs | 同一 inode 的权限与通知事务 | 新增 metadata family 前拆分 |
| `kernel/src/task/process_table/signal.rs` | 1270 | task | graph 下的生成与子进程通知事务 | 新增独立 signal producer 前拆分 |

## Change contract

- 修改前确定所属 module、state owner、interface 与依赖变化，以及 error、exit、interrupt cleanup。
- 未列入 dependency matrix 的跨 module 依赖一律失败；同 module 内引用不构成跨 seam 依赖。
- 新 global、lock、Atomic、Once、cache、flag 或 unsafe 必须在领域契约记录 owner、安全证明与缺失时的具体失败。
- scoped interface 只能用 `cargo run -p architecture-check -- --write-interface` 生成到 [generated baseline](generated/architecture-interface.txt)，禁止手工编辑。
- 文档、单元测试、性能测试和运行时 gate 必须随契约变化同步维护；不得放宽围栏或阈值来接受实现错误。
