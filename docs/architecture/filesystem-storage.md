# 文件系统与存储当前架构

## 当前设计

- OFD allocation 在 filesystem 打开副作用前准备，完整初始化才进入打开/关闭生命周期；
  失败不会析构未成立的 OFD。fd slot 同样先预留，完成可失败步骤后原子发布。
  socket 创建/accept 同样先准备 facade/OFD/匿名 inode；accept 在消费连接前预留 fd。
  pipe2/socketpair 的输出先写 reserved numbers，再在原表的一次 guard 内发布全部 fd；
  失败取消原表 slots，既不暴露半对 fd，也不关闭后来重用同号 fd 的另一个 Thread。实际失败与并发回归纳入统一门禁。
- 每个 OFD 都保留真实 VFS opened entry；canonical dentry 表达 filesystem 目录关系，实际 mount view 表达挂载关系，anonymous entry 的标签仅供 readlink、从不按 pathname 展开。`DentryIndex` 以 exact ordered membership
  连接 register、rename/unlink 与 final Drop，路径解析不再按组件扫描全部
  live opened entries。index node 只持有 Weak；namespace mutation 在锁外 upgrade，
  且被替换 parent 与临时 strong pin 均在 index lock 外析构，避免 final Drop 递归取锁。
  OpenFileDescription 拥有 backend、offset、status flags 与 descriptor reference
  consequence；fd table 只拥有 slot 和 descriptor flags。二者属于 `file` module（位于 `fs` 与 `net` 之上），VFS 的
  advisory lock 只经 `LockHolder` 识别持锁者，不认识 OFD。
- ext4 是当前可写 root filesystem，只接受 e2fsprogs 1.47.4 `mke2fs -t ext4` 默认 profile（extent、
  64bit、flex_bg、metadata_csum、dir_index、orphan_file、JBD2 `CSUM_V3`）；其余 feature 在 mount 时拒绝。
  inode、directory mutation、link count、allocation 与 JBD2 metadata journal 在 filesystem owner 内闭合。
- 每个 metadata 结构（superblock、group descriptor、bitmap、inode、extent node、directory leaf/dx block、
  orphan block、xattr block）读入时校验 crc32c，写出时由同一 owner 重新 seal。
- ext4 filesystem owner 持有按 filesystem block number 标识的 64-entry metadata cache，覆盖 directory、
  htree、extent node 与 orphan file block；缓存只保存完整 block image，固定容量按 LRU reclaim。
- logical block 只由 `ExtentTree` 映射：inode 内 root 加 block node，lookup 不分配；insert 合并相邻
  extent、按需分裂叶/索引并增高，truncate 自尾部删除并回收空 node。
- 目录以单 block 线性布局起步，满后转换为 half_md4 htree（最多两层 index）；readdir 按 hash 顺序推进。
- 所有类型的 open-unlinked inode（包括 rmdir/rename 覆盖的目录）记入同一 orphan file slot；目录逻辑长度立即清零，实际 extent 与 inode bitmap 保留至最后引用释放。已删除的 cwd/open directory 不允许创建新 entry。final Drop 只尝试
  非阻塞取得 mutation owner，竞争时由
  filesystem 合并 retry，下一次 task mutation 前回收一个已无 live Weak identity 的 orphan。
- ext4 写路径是 Linux `data=ordered` + group commit：`write`/`create`/`rename` 等 mutation 只把
  staged metadata 与文件数据并入内存中的 running transaction 后返回；metadata 经 JBD2 原子提交，
  数据不进 journal、在 commit record 之前直接写回 home。running transaction 由 `fsync`/`execution::sync`、
  journal 容量、16 MiB 数据上限或 5 秒年龄触发提交；每个 ext4 filesystem 的写回内核线程负责空闲期的
  5 秒上限。backup superblock/GDT 保持 mkfs 时的内容，由 e2fsck 维护。
- JBD2 commit 在 commit record 前持久化 dirty marker、descriptor 与 data image；mount replay 后先从
  primary home blocks 重新发布 superblock/GDT runtime owner，再执行 orphan recovery 与一致性扫描。
- page cache 唯一拥有 shared file page identity、dirty/writeback 状态和 reclaim cursor；VMA 与 filesystem 通过 shared-page seam 交互。
- 内核只挂载根文件系统（`fs::mount_root`，source 为 `/dev/<disk>`）与 `/dev` 上的 devtmpfs（Linux
  `CONFIG_DEVTMPFS_MOUNT`）；`/proc`、`/sys`、`/dev/pts`、`/run`、`/tmp`、`/dev/shm` 由 init 的
  `/etc/init.d/rcS` 经 `mount(2)` 挂载。文件系统类型是 `fs::FileSystemType` 注册表中的成员
  （Linux `register_filesystem`）：`ext4`、`proc`、`sysfs`、`devpts`、`devtmpfs`、`tmpfs` 在 `init_vfs`
  登记，`mount(2)` 的 `filesystemtype`、`rootfstype=` 与根挂载的候选都从同一张表解析。类型自己解析
  `data` 选项并拒绝不认识的 key；VFS 只负责发布挂载，发布失败时调用 `FileSystem::shutdown`。
  每个实例都分配新的 filesystem instance id（Linux `get_anon_bdev`），因此各挂载有独立 `st_dev` 与 VFS
  identity，不形成第二套 namespace 或对象状态。
- 内存型 regular file（tmpfs、memfd）的真实 atime/mtime/ctime 由共享 TimeOwner tuple 保存，metadata 与 writable mmap page 读取同一 owner；显式 mtime 可以是过去/未来，不以其最大值伪造 ctime。
- 内存型 regular file（tmpfs、memfd）的内容由 `fs::MemoryFile` 单份持有：稀疏页映射即文件内容，
  不经 page cache。`Inode::data_backing` 区分 `PageCache`（持久文件）、`Snapshot`（procfs 即时生成）与
  `Memory`；read/write/truncate/fallocate/mmap 按它分派。页在文件失去最后一个 link 与最后一个打开/映射引用
  时随 `Arc` 释放并立即归还 `size=` 配额；page cache 的全局 registry 持有 inode 会让已删除文件的内容滞留到
  下一次 `execution::sync`，并让热数据占两份内存，所以内存型文件不得经过它。
- tmpfs 的目录项按名字索引并按单调 cookie 迭代（`getdents` cursor 是上一项 cookie，`.`/`..` 固定为 1/2），
  迭代中途创建或删除不会让位置漂移。`size=`/`nr_blocks=` 限制数据页，`nr_inodes=` 限制 inode；
  `mode=`/`uid=`/`gid=` 决定根目录。`/dev/shm` 是 devfs 注册表声明的空目录，由 init 在其上挂载 tmpfs。
  StorageBudget 同锁拥有 page/inode limits 与占用，remount 整体提交两类上限；失败保持旧值。
  statfs 读取同一次 quota snapshot。与固定 Linux shmem 一致，unlimited 挂载不能在 remount 追溯加限。
- 挂载属性（`ro`、`nosuid`、`nodev`、`noexec`，位值与 `statfs.f_flags` 的 `ST_*` 一致）由 VFS 按挂载保存：
  namespace 变更（create/unlink/rename/link/symlink）、chmod/chown/utimes 与以写方式打开得到 `EROFS`；
  nodev 挂载上的设备节点不可 open（`EACCES`）；noexec 挂载上的文件不可 exec 或映射为可执行；nosuid 挂载上
  exec 忽略 set-id 位。`MS_REMOUNT` 在同一把锁内替换属性：转为 read-only 时仍有以写方式打开的 OFD 返回
  `EBUSY`（OFD 在创建时登记、释放时撤销写者），随后 filesystem 应用 `data`（失败则还原属性），最后同步。
  `/proc/mounts` 与 `statfs` 反映同一 actual view 属性。NOATIME/NODIRATIME/RELATIME/strict
  由实际路径消费，读取 cache hit/miss 不产生不同策略；时间更新只改 atime，失败不破坏读取。
  同设备多个挂载共享一个 actual filesystem/physical claim，每个 view 独立 source/flags。
- Mount namespace 只持实际 forest root；强 children/弱 parent 表达唯一 topology。final Drop 移交
  预留 payload，既有 VFS gate admission 与独立退休 worker 摘索引，锁外先 dput 根再 shutdown FS。
  普通 umount 判 busy 并等待实际资源清理；lazy detach 断开普通子边，旧 FD 继续访问自己的 view。
  当前 caller FS root 转 RO 前也验证 caller namespace membership 与不可拆分限制。
  MNT_EXPIRE 两次空闲尝试，普通 path release 清标。mmap pin actual OFD，使实际 writer/backend 保活。
- Canonical inode 的 i_writecount 互斥 writer 与执行 deny-write；hardlink/bind/namespace alias 共用。
  主程序租约与 proc exe 属于实际 mm，解除全部 LOAD 不提前释放；解释器只在加载期拒绝写入。
  exec 取得租约后重取权限 tuple，foreign namespace mount 的 set-ID 不可信。
- 块设备经 fs 块命名空间发布为 devfs `S_IFBLK` 节点；`mount` 的 source 经 `lookup_bdev` 解析为设备号，
  同一块设备只能承载一个已挂载实例。
- directory iteration 由 inode adapter 从 opaque cursor 直接推进：ext4 线性目录的 cursor 是下一 record byte
  offset，htree 目录的 cursor 是 hash 位置，内存型 adapter 使用 ordinal cookie；VFS 不物化完整目录，`getdents64` 只编码一个有界 batch。
- close、dup replacement、CLOEXEC 与 SCM receive 遵守 reserve/detach/publish 顺序，可能析构或通知的 consequence 在 fd-table lock 外执行。
- VFS lookup 共享持有实际父目录锁，create/unlink/rename 独占持有相关目录项；等待 FUSE 不持全局拓扑锁。
  mount 穿越、位置发布与整条路径投影只短暂持有 namespace gate，remount 的后端准备在 gate 外。
  VFS `openat(O_CREAT)` 在同一次父目录独占持有内原子选择 existing winner 或 create commit；
  无 `O_EXCL` 的并发 append 不会因另一个 creator 先提交而误报 `EEXIST`。

## Known limits

- 持久存储是固定 ext4/JBD2 profile；附加块设备与分区可经 `mount(2)` 挂载；分区表解析见 `block::partition_table`（纯函数，host 单测覆盖 MBR 逻辑分区链、GPT CRC 与备份表回退）。
- FIFO、字符/块设备节点由 `Inode::mknod` 创建（ext4 把设备号按 Linux 旧/新编码存入 `i_block`，无 extent tree；
  tmpfs 存为 `Body::Special`）。FIFO 的内核 Pipe 由 `fs::fifo` 按 `(filesystem, inode)` 绑定，只持 `Weak`，
  registry 使用可跨 allocation 的 TaskMutex。真实 endpoint admission 与最后 close 在 PipeState 同锁更新 Fresh/Live/Retired，临时 Arc/Weak pin 不复活旧 ring，最后 close 锁外释放 ring；重开取得新 session、保留 inode identity。打开的 FIFO
  是一个持有 0~2 个 endpoint 的 `DeviceFile`，所以双向读写、
  两路 poll/epoll source 与阻塞都走设备 seam，写入无 reader 时经 `DeviceError::BrokenPipe` 投递 `SIGPIPE`。VFS 发布 opened entry 时把任何文件系统里的块设备节点包装成 `BlockSpecial`：
  metadata/权限仍归节点所属文件系统，字节 I/O、容量、缓存身份、ioctl 与 fsync 统一落到 `BlockNode`。
- 原始块 I/O 经 page cache 缓冲，但 ext4 直接读写块层，二者不共享缓存，所以一块盘要么被挂载（节点只读、
  不缓冲）要么可写打开：挂载与“以写方式打开”由同一物理盘的 extent claim owner 互斥，挂载前写回并逐出全部重叠别名的缓冲页。
  只读打开并读取已挂载的盘看到的是已提交到块层的数据，不含 ext4 尚在内存 transaction 里的元数据。
- inotify 实例（`fs::Inotify`）是 anonymous `DeviceFile`：自己的 watch 列表与有界事件队列，阻塞 read、poll/epoll、
  `FIONREAD` 走设备 seam。全局 watch 注册表按 inode 身份索引；变更点（VFS 的 create/unlink/rename/link，
  OFD 的 open/read/write/close，属性类 syscall）调用 `fs::notify_*`，没有任何 watch 时只是一次 Relaxed 原子读取。
  文件事件同时投递给文件自身的 watch 与父目录的 watch（带名字）；事件生成在变更成功之后，不改变变更本身的结果。
- 块设备枚举（`/proc/partitions`、`/sys/{class,}/block`、`/sys/dev/block`）每次读取都从设备注册表取快照，不缓存；
  注册表只追加，下标即稳定身份（`sysfs_block`）。
- 只读挂载 ext4 与 Linux 一致：journal 照常重放，但不置 `RECOVER`、不回收 orphan；`remount,rw` 经
  唯一 `FileSystem::remount` 先校验 options 再补做写前准备（幂等）；仅全部成功才发布完成位。
  首次挂载与 remount 接受实际报告的 `data=ordered`，其他 data 模式拒绝。
  根挂载从默认 relatime 属性加入 `ro` 位；init 的 `remount,rw /` 转为可写时保留实际 atime 策略。
- 分区是整盘 `BlockDevice` 上的 4 KiB 对齐区间（`PartitionDevice`）；整盘与分区的 mount/writer
  共享物理范围 claim，禁止重叠别名同时写或挂载，不重叠分区可同时挂载。page cache 仍按 bdev
  身份独立；领取 writer 时逐出其他重叠别名且阻止它们重新缓存，只读别名
  绕过 cache 读取已写回的块层内容，不能看到 writer 自有 cache 中尚未写回的内容。
- tmpfs 没有 swap：数据页只受 `size=` 与物理内存限制，`MAP_SHARED` 触碰超出配额的洞得到 `SIGBUS`
  （与 Linux 一致）；可写映射建立时更新 `st_mtime`，之后的 store 不再经过内核。
- 不支持 `huge=`、`mpol=`（没有大页与 NUMA 子系统）与 xattr/ACL。bind/move/propagation 挂载、`MS_SYNCHRONOUS`、
  `MS_MANDLOCK`、`MS_NOSYMFOLLOW` 与同目录堆叠挂载返回 `EINVAL`/`EBUSY`。
- tmpfs 目录树释放（umount 或最后一个引用消失）用常数栈的循环拆除，深度不受限。
- 已删除文件的 page cache 在 unlink/rename 覆盖或最后一个打开条目释放时随 inode 逐出（Linux `evict_inode`），
  ext4 的 orphan 回收不再等待下一次 `execution::sync`；仍被打开或映射的文件保持存活。
- 没有通用 block scheduler 或多个可热插拔持久卷策略。
- 已返回的写入在 `fsync`/`execution::sync` 前最多可能丢失 5 秒（与 Linux `commit=5` 一致）；块分配仍发生在
  `write` 时，没有 delayed allocation。
- ext4 磁盘保存纳秒时间戳与 crtime，但 VFS metadata 只投影非负秒数；`utimensat` 的纳秒部分与
  `statx` birth time 不对用户可见。
- orphan file 满时 open-unlinked 返回 `NoSpace`，不回退到 legacy orphan chain。
- ext4 的大扩展属性已接入原生 EA_INODE 源码：属性 entry 指向隐藏磁盘 inode，值使用实际
  extent tree；atime 保存值 hash，ctime/version 保存引用计数。引用按 body entry/物理 external
  block entry 计算，COW 和最后回收与父 inode 共用现有 journal。新镜像启用 ea_inode；旧镜像
  不自动升级。64 KiB 值、e2fsprogs 导入、remount、失败退费及三个 crash barrier 回归已写。
  值读取是冷路径，新增实际 64 KiB producer 诊断；既有 mapping/write 热路径门槛保持原值。
- 无 `largedir`：htree 最多两层 index，满后目录插入返回 `NoSpace`。htree readdir 在 batch 边界上，
  同一 hash position 的剩余 entry 会与停止点共享 cursor，可能被重放但不会遗漏。
- runtime 只更新 primary superblock/GDT；backup 副本由 e2fsck 修复，没有在线 resize。

用户 pathname 使用 Thread fs context 的同锁 root/cwd snapshot。VFS 的 LookupOrigin 显式携带
root 与 relative start，所有解析/修改、AF_UNIX、swap、exec 的脚本与 ELF interpreter 都走同一
path_lookup；绝对 symlink 重新从 caller root 开始，dotdot 在 mount escape 之前 clamp。
chroot 更改共享 root，保留 cwd 和已有 FD。getcwd 投影 root-relative 链，不可达时保留 raw
`(unreachable)`；proc fd/exe/root/cwd 使用 viewer root，main Thread 退出后 root/cwd 不再可读。

proc 控制文件支持 `open(O_TRUNC)` 与合法的 truncate 属性操作；依 Linux setattr_copy，
只更新时间，保持固定 i_size 与 map/sysctl 内容。proc/nsfs 重复 lookup 和 ioctl producer
复用同一 live inode；nsfs 的普通属性更新与 namespace identity 分离。registry 只持 Weak，
关闭全部实际 inode 后不延长 namespace 或其 quota charge 的寿命。
