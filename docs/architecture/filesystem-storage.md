# 文件系统与存储当前架构

## 当前设计

- VFS opened entry 表达 pathname identity；`OpenedIndex` 以 exact ordered membership
  连接 register、rename/unlink 与 final Drop，路径解析不再按组件扫描全部
  live opened entries。index node 只持有 Weak；namespace mutation 在锁外 upgrade，
  且被替换 parent 与临时 strong pin 均在 index lock 外析构，避免 final Drop 递归取锁。
  OpenFileDescription 拥有 backend、offset、status flags 与 descriptor reference
  consequence；fd table 只拥有 slot 和 descriptor flags。
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
- open-unlinked inode 记入 orphan file slot；final Drop 只尝试非阻塞取得 mutation owner，竞争时由
  filesystem 合并 retry，下一次 task mutation 前回收一个已无 live Weak identity 的 orphan。
- ext4 写路径是 Linux `data=ordered` + group commit：`write`/`create`/`rename` 等 mutation 只把
  staged metadata 与文件数据并入内存中的 running transaction 后返回；metadata 经 JBD2 原子提交，
  数据不进 journal、在 commit record 之前直接写回 home。running transaction 由 `fsync`/`sync`、
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
- 内存型 regular file（tmpfs、memfd）的内容由 `fs::MemoryFile` 单份持有：稀疏页映射即文件内容，
  不经 page cache。`Inode::data_backing` 区分 `PageCache`（持久文件）、`Snapshot`（procfs 即时生成）与
  `Memory`；read/write/truncate/fallocate/mmap 按它分派。页在文件失去最后一个 link 与最后一个打开/映射引用
  时随 `Arc` 释放并立即归还 `size=` 配额；page cache 的全局 registry 持有 inode 会让已删除文件的内容滞留到
  下一次 `sync`，并让热数据占两份内存，所以内存型文件不得经过它。
- tmpfs 的目录项按名字索引并按单调 cookie 迭代（`getdents` cursor 是上一项 cookie，`.`/`..` 固定为 1/2），
  迭代中途创建或删除不会让位置漂移。`size=`/`nr_blocks=` 限制数据页，`nr_inodes=` 限制 inode；
  `mode=`/`uid=`/`gid=` 决定根目录。`/dev/shm` 是 devfs 注册表声明的空目录，由 init 在其上挂载 tmpfs。
- 挂载属性（`ro`、`nosuid`、`nodev`、`noexec`，位值与 `statfs.f_flags` 的 `ST_*` 一致）由 VFS 按挂载保存：
  namespace 变更（create/unlink/rename/link/symlink）、chmod/chown/utimes 与以写方式打开得到 `EROFS`；
  nodev 挂载上的设备节点不可 open（`EACCES`）；noexec 挂载上的文件不可 exec 或映射为可执行；nosuid 挂载上
  exec 忽略 set-id 位。`MS_REMOUNT` 在同一把锁内替换属性：转为 read-only 时仍有以写方式打开的 OFD 返回
  `EBUSY`（OFD 在创建时登记、释放时撤销写者），随后 filesystem 应用 `data`（失败则还原属性），最后同步。
  `/proc/mounts` 与 `statfs` 反映这些属性。`MS_NOATIME` 等访问时间策略被接受：内核不在读取时维护 atime。
- `umount2` 按 Linux 顺序拆除：VFS 判忙并摘下挂载 → 写回并逐出该实例的 page cache →
  `FileSystem::shutdown`（ext4 提交 journal 并让写回线程返回）。mmap 映射 pin 住来源打开条目（Linux
  `vm_file`），因此仍有映射的挂载保持忙。
- 块设备经 fs 块命名空间发布为 devfs `S_IFBLK` 节点；`mount` 的 source 经 `lookup_bdev` 解析为设备号，
  同一块设备只能承载一个已挂载实例。
- directory iteration 由 inode adapter 从 opaque cursor 直接推进：ext4 线性目录的 cursor 是下一 record byte
  offset，htree 目录的 cursor 是 hash 位置，内存型 adapter 使用 ordinal cookie；VFS 不物化完整目录，`getdents64` 只编码一个有界 batch。
- close、dup replacement、CLOEXEC 与 SCM receive 遵守 reserve/detach/publish 顺序，可能析构或通知的 consequence 在 fd-table lock 外执行。
- VFS `openat(O_CREAT)` 在 namespace mutation owner 内原子选择 existing winner 或 create commit；
  无 `O_EXCL` 的并发 append 不会因另一个 creator 先提交而误报 `EEXIST`。

## Known limits

- 持久存储是固定 ext4/JBD2 profile；附加块设备与分区可经 `mount(2)` 挂载；分区表解析见 `fs::partition_table`（纯函数，host 单测覆盖 MBR 逻辑分区链、GPT CRC 与备份表回退）。
- FIFO、字符/块设备节点由 `Inode::mknod` 创建（ext4 把设备号按 Linux 旧/新编码存入 `i_block`，无 extent tree；
  tmpfs 存为 `Body::Special`）。FIFO 的内核 Pipe 由 `fs::fifo` 按 `(filesystem, inode)` 绑定，只持 `Weak`，
  生命周期由 endpoint 决定；打开的 FIFO 是一个持有 0~2 个 endpoint 的 `DeviceFile`，所以双向读写、
  两路 poll/epoll source 与阻塞都走设备 seam，写入无 reader 时经 `DeviceError::BrokenPipe` 投递 `SIGPIPE`。VFS 发布 opened entry 时把任何文件系统里的块设备节点包装成 `BlockSpecial`：
  metadata/权限仍归节点所属文件系统，字节 I/O、容量、缓存身份、ioctl 与 fsync 统一落到 `BlockNode`。
- 原始块 I/O 经 page cache 缓冲，但 ext4 直接读写块层，二者不共享缓存，所以一块盘要么被挂载（节点只读、
  不缓冲）要么可写打开：挂载与“以写方式打开”由 `BlockNode` 的原子字互斥，挂载前写回并逐出该盘的缓冲页。
  只读打开并读取已挂载的盘看到的是已提交到块层的数据，不含 ext4 尚在内存 transaction 里的元数据。
- 块设备枚举（`/proc/partitions`、`/sys/{class,}/block`、`/sys/dev/block`）每次读取都从设备注册表取快照，不缓存；
  注册表只追加，下标即稳定身份（`sysfs_block`）。
- 只读挂载 ext4 与 Linux 一致：journal 照常重放，但不置 `RECOVER`、不回收 orphan；`remount,rw` 经
  `FileSystem::make_writable` 补做（幂等）。根以 `ro` 启动参数挂载后由 init 的 `remount,rw /` 转为可写。
- 分区是整盘 `BlockDevice` 上的 4 KiB 对齐区间（`PartitionDevice`），自己有独立的 `BlockNode`：挂载与写者互斥
  按节点独立；整盘与分区各有各的 page cache，重叠区域不保证一致（与 Linux 相同）。
- tmpfs 没有 swap：数据页只受 `size=` 与物理内存限制，`MAP_SHARED` 触碰超出配额的洞得到 `SIGBUS`
  （与 Linux 一致）；可写映射建立时更新 `st_mtime`，之后的 store 不再经过内核。
- 不支持 `huge=`、`mpol=`（没有大页与 NUMA 子系统）与 xattr/ACL。bind/move/propagation 挂载、`MS_SYNCHRONOUS`、
  `MS_MANDLOCK`、`MS_NOSYMFOLLOW` 与同目录堆叠挂载返回 `EINVAL`/`EBUSY`。
- tmpfs 目录树释放（umount 或最后一个引用消失）用常数栈的循环拆除，深度不受限。
- 已删除文件的 page cache 在 unlink/rename 覆盖或最后一个打开条目释放时随 inode 逐出（Linux `evict_inode`），
  ext4 的 orphan 回收不再等待下一次 `sync`；仍被打开或映射的文件保持存活。
- 没有通用 block scheduler 或多个可热插拔持久卷策略。
- 已返回的写入在 `fsync`/`sync` 前最多可能丢失 5 秒（与 Linux `commit=5` 一致）；块分配仍发生在
  `write` 时，没有 delayed allocation。
- ext4 磁盘保存纳秒时间戳与 crtime，但 VFS metadata 只投影非负秒数；`utimensat` 的纳秒部分与
  `statx` birth time 不对用户可见。
- orphan file 满时 open-unlinked 返回 `NoSpace`，不回退到 legacy orphan chain。
- 无 `largedir`：htree 最多两层 index，满后目录插入返回 `NoSpace`。htree readdir 在 batch 边界上，
  同一 hash position 的剩余 entry 会与停止点共享 cursor，可能被重放但不会遗漏。
- runtime 只更新 primary superblock/GDT；backup 副本由 e2fsck 修复，没有在线 resize。
