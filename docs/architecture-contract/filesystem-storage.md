# 文件系统与存储契约

## Owner

- VFS namespace/inode 拥有 pathname identity；OpenFileDescription 拥有 backend、file position、status flag 与 descriptor reference consequence。
- `OpenedIndex` 是 live opened-entry lifecycle/path membership 的唯一 owner；key 以
  parent inode identity/name/inode identity 为 namespace 前缀，以 Arc allocation identity 区分
  重复 lookup。register 只做 ordered insert，rename/unlink 只访问精确前缀，
  `OpenedFile::drop` 在 storage 解配前精确撤销 membership；禁止恢复
  `Vec<Weak<OpenedFile>>` 和任何 lazy retain sweep。
- `FilePosition` 是 OFD position 的唯一 lock owner：sequential read/write、`lseek` 与 `getdents64`
  必须在单次 `with_position` 临界区内完成依赖 position 的完整操作；`sendfile` 必须通过
  `with_positions` 的稳定地址全序取得两个不同 OFD，禁止 syscall 直接取得 raw position lock。
- FileDescriptorTable 独占 slot、FD_CLOEXEC、reservation、publication 与 lowest-free index；fd slot 使用
  heap-owned 7/7/6-bit sparse radix，fullness summary 只能由同一 occupancy transition 更新。lookup、
  replace 与 take 固定三层，iteration/fork 只访问 materialized branch/chunk，禁止按最高 fd 物化或复制
  dense `Option<FileDescriptor>` table。RV64 reviewed payload 为 table inline 24 B、root/branch 各
  1040 B、64-slot FileDescriptor chunk 1024 B；仅 fd 0 与 1,048,575 时 heap payload 为 5168 B，
  全物化时 metadata/chunk payload 上限分别为 134,160 B/16,777,216 B（不含 allocator header）。
- ext4 owner 独占 inode/directory/link/allocation mutation；packed disk value 定义与字段保持
  `fs::ext4` parent-private，`fs::ext4::layout` 只封装定长 decode/encode 与 raw byte access，
  `fs::ext4::block_io` 封装 filesystem/device block 换算，`fs::ext4::inode` 独占 inode identity 与
  VFS projection，`fs::ext4::extent` 独占 logical-block mapping，`fs::ext4::htree` 独占 dx index，
  `fs::ext4::metadata_csum` 独占全部 metadata checksum 公式；`Ext4FileSystem` 的 64-entry metadata
  block cache 独占 directory/htree/extent/orphan block identity 与 LRU reclaim；JBD2 journal 独占
  transaction/commit/replay；page cache 独占 cached page lifecycle。
- ext4 写路径固定为 Linux `data=ordered` + group commit：每次 mutation 的 handle overlay 独占本次
  staged metadata/data、释放的 block range 与 allocation dirty-group bitset；bitset 必须在 handle 发布前
  按 group count fallible reserve，OOM 不得开始 mutation。`MutationGuard::commit` 一次性物化 primary
  superblock 与每个 dirty descriptor block 后，以只移动 tree node 的方式把 overlay 原子并入唯一
  running transaction；并入后 mutation 对内存可见且不可回滚，并入前的任何失败都只丢弃 overlay。
- running transaction 独占全部未提交 metadata、ordered data 与未提交释放 range。metadata 经 JBD2
  原子提交；data 不进 journal，必须在 commit record 之前写回 home 并共用同一 durability barrier。
  未提交事务释放的 block 在提交前不得重新分配，否则崩溃后旧文件会指向新文件已写回的数据。
- running transaction 只在以下时机提交：handle 并入后 metadata 超过单个 journal transaction 容量
  （先提交 running，再并入）、staged data 超过 16 MiB、年龄超过 5 秒、`fsync`/`fdatasync`/`sync`。
  每个 ext4 filesystem 在 mount 时创建一个写回内核线程（对应 Linux jbd2）：running transaction
  由空变非空时经 `TaskEvent` 唤醒，睡到开始时刻加 5 秒后在 mutation owner 下提交；没有未提交
  mutation 时线程无限期阻塞，不产生周期唤醒。`shutdown` 提交 running transaction 后置位
  `stopping` 并 signal，线程返回并释放对 filesystem 的引用。
- `fs::mount` 的 `MOUNT_TRANSACTION` 串行化完整 mount/umount 事务；ext4 实例一经创建即回放 journal，
  “设备未挂载”检查与发布之间不得并发。VFS `unmount` 在 mounts 锁内判忙：有子挂载，或挂载根
  `Arc` 引用数超过“挂载记录 + 调用者”（打开文件、cwd、mmap 与更深的打开条目都经 parent 链持有它）
  即 `Busy`；检查与摘除和 `enter_mount` 串行。
- `RegularFileWrite` 的 write-sequence 与 operation gates 共同独占一次 syscall 的 position、append placement、storage transaction 和 resident-cache publication 顺序。
- VFS namespace mutation 与 ext4 live-state transaction 使用 `TaskMutex` 逻辑 owner；其内部
  spin gate 只发布 `Available/Held/Handoff(ticket)` 与预分配 waiter 链，logical guard 可以跨
  block I/O 和 task handoff，但不得保留 spin guard。竞争 waiter 必须进入 scheduler `Blocked`，
  unlock 在短锁内把 owner 直接交给最旧 ticket、锁外 exact wake；禁止恢复 spin/yield polling。
- page-cache `operation`/`write_sequence` 同样是可跨 cache fill、writeback 与 storage mutation
  保活的 task-only blocking owner；只有 resident page map 与全局 registry 的短临界区使用 spin。
- 每个 `MemoryFile`（memfd 与 tmpfs regular file 共用）的 `state` 同时拥有文件长度、稀疏页与 seal mask；
  `ftruncate`/write/fallocate 和 `F_ADD_SEALS` 必须在该 owner 内线性化，禁止用独立 atomic seal 留出
  seal 与 resize 的竞态窗口。`write_sequence`（外层，整次 write/append/truncate/fallocate，持有期间会做用户
  态拷贝）先于 `state`（内层，page fault 只取它）；`published_length` 是 `state` 的无锁只读投影，只在持有
  `state` 时更新。页配额由 `PageBudget` 原子预留，页在最后一个 `Arc` 释放时归还。
- 挂载属性与“以写方式打开的 OFD 数”同在 VFS 的 `MountAttributes`，由 `mounts`/`root_fs` 锁一起保护：
  `begin_write_open` 与 `remount,ro` 的判忙互斥，OFD 的 `WriteOpen` 在 Drop 时撤销。读路径与 `read`/`write`
  热路径不取该锁。
- `BlockNode.claim` 是“已挂载”标志与写者计数的唯一原子字：`begin_writer`（OFD 创建）、`begin_mount`、
  `end_writer`（OFD Drop，不阻塞）、`end_mount` 都对它做 CAS/原子更新；块设备在 page cache 中的身份由
  `Inode::page_cache_id` 给出（与 devtmpfs 实例无关），挂载前经 `page_cache::evict_cached` 写回并逐出。
- `/dev/kmsg` 的唤醒经 logger 发布 `cpu` deferred vector → `fs::mem` 的合并 readiness Pipe；logger 在任何
  上下文（含 hardirq）都只做原子发布，不取 scheduler 锁。`KmsgReader.gate` 序列化同一 OFD 的 reader，
  `cursor` 只在整条 record 交给用户后推进。
- `fs::FileSystemType` 注册表只追加、在 `init_vfs` 发布；`MOUNT_TRANSACTION` 串行化 mount/umount 事务。
  tmpfs 的目录结构变更先取 `Shared::namespace`，再取涉及的 inode 状态锁（rename 的两个目录按 inode 编号
  顺序）；只读路径只取单个 inode 状态锁，因此不存在两个多锁路径互相等待。inode 编号只增不复用。

## Interface

- filesystem 只通过 block seam 使用 driver，通过 shared-page seam 使用 memory，通过 unified backend façade 接入 pipe/socket/device。
- `memfd_create` 只发布 anonymous regular OFD；`MFD_ALLOW_SEALING`、`F_ADD_SEALS`、
  `F_GET_SEALS`、`ftruncate` 与 `MAP_SHARED` 沿既有 fd/inode seam 工作（内容在 `MemoryFile`，经
  `Inode::data_backing` 分派，不经 page cache），不注册 pathname 或引入私有 shared-memory ABI。当前 seal 子集为 `SEAL|SHRINK|GROW`，未支持的 write/hugetlb
  语义明确返回错误。
- `openat(O_CREAT)` 的 final lookup 与 create 必须在同一个 VFS namespace mutation transaction
  内完成：存在且无 `O_EXCL` 时打开 winner，存在且有 `O_EXCL` 时返回 `EEXIST`，不存在时创建。
  禁止锁外先 lookup 再调用独立 create；该双阶段会让并发普通 append 错误收到 `EEXIST`。
- fd reservation 在 lookup/procfs/fork/close 前不可见；`recvmsg` 的 fd number 与全部关联 metadata
  copyout 成功后才能整批 publish，任一失败必须在 fd-table lock 外完成全部 reservation cleanup。
- OFD position 的推进只在对应 operation 已产生进度后发生；copyout 失败不得发布 `getdents64`
  position。`lseek` 结果必须能由 Linux signed `loff_t` 表示，不能把超出 `i64::MAX` 的值
  截断为负 syscall return。
- directory inode 只暴露 `read_directory(cursor, visitor)` 单轨 interface；`cursor` 是 adapter-owned
  opaque `d_off`，visitor Stop 不消费当前 entry。禁止恢复全量 `list()` 后按 OFD ordinal 截取。
  ext4 线性目录使用下一 record 的 byte offset，并只从 cursor 所在 block 开始；并发 mutation 令旧
  cookie 落入合并 record 时，在该 block 内向后对齐。htree 目录使用 Linux 64-bit hash position
  （`2 + (major << 31 | minor >> 1)`，EOF 为 `i64::MAX`），从 cursor 所在 collision run 起按 hash 输出。
- `getdents64` 每批最多一次性预留用户容量与 64 KiB 上限的较小值；不得在 entry loop 内扩容。
  filesystem/编码/OOM/copyout 失败均不得发布候选 cursor，只有完整 copyout 后才在同一 OFD
  position transaction 提交。复杂度 gate 的 128-entry/4-entry-batch 模型要求零次全量 list、
  entry 物化不超过 128、output reserve 不超过 32，ext4 block read 不超过 block 数加 batch 数。
- pathname-backed OFD 必须保留 opened-entry identity；rename/unlink 不能把打开对象退化为字符串路径。
- opened index 的 exact node 只保存 `Weak<OpenedFile>`，不得增加 Arc cycle；mutation
  只在 index lock 内复制 exact key/Weak，再在锁外 upgrade。成功的临时 Arc 排除 final
  Drop，失败则不得解引用，并由已开始的 final Drop 精确撤销节点。dup/fork 仍共享同一
  OFD/OpenedFile，只有最后 Arc lifetime 结束时撤销节点。
- index lookup 只在锁内复制 exact key/Weak；upgrade pin、rename 替换出的旧 parent Arc
  必须在 index lock 外析构，否则最后 strong ref 会经 `OpenedFile::drop -> unregister`
  递归取得同一锁并在单 CPU 死锁。
- packed disk layout、journal block、device adapter 与 syscall UAPI 不得穿过 VFS seam。
- directory、htree、extent node 与 orphan block 读取只能经 filesystem-owned metadata block seam；journal stage 成功后必须
  在释放 journal lock 前更新或失效同 block cache identity，commit/home write 保持新 image，abort 与
  commit failure 必须失效 staged identity，truncate/free 必须在 block 可重用前失效旧 identity。cache
  miss admission 的 allocation 失败不得发布 partial entry；generation 改变时不得发布过期 miss。
- journal commit 必须把 `Journal` 与 immutable staged write view 从短 spin owner 中 loan 出来；
  descriptor/data flush、home checkpoint 与 clean-state write 全部在 owner lock 外执行。commit
  期间 reader 只短暂取得 staged view，cache miss 可继续访问 home device；禁止把 block I/O
  重新放回 journal spin guard。commit failure 必须清空 metadata cache 并把 journal 标记为
  fail-stop，后续 mutation 不得另走无 journal 兼容路径；`fsync`/`sync` 报告该提交失败的 EIO。
- mount journal replay 若更新了 home blocks，必须在任何 superblock home write、orphan reclaim 或
  consistency scan 前，从 primary home blocks 重新 decode/validate superblock 与完整 GDT，验证
  immutable topology 未改变、清空 replay 前 cache identity，再一次性发布 runtime owner。禁止让
  replay 前的 `superblock/groups` 快照覆盖或解释 replay 后的 bitmap/inode state。
- ext4 inode mutation 只能使用锁外 `InodeMutation` working copy；普通 inode spin guard 只允许
  取得或发布一个完整 `Ext4InodeDisk` snapshot，不得跨 journal/block I/O。working copy 的类型
  lifetime 必须借用 `MutationGuard`，所以全部 live inode 发布必然发生在 commit 消费并释放
  filesystem mutation owner 之前；禁止恢复返回 `MutexGuard` 或依赖函数退出后的延迟发布。
- mount consistency scan 每轮只在短 spin 临界区复制一个 group descriptor，再在锁外读取
  block/inode bitmap；即使 filesystem 尚未发布，也不得让普通 spin guard 跨 DriverIo sleep。
- logical-block mapping 只能由 `ExtentTree` 拥有：`lookup` 不分配，`map_block_sparse` 唯一委托
  `lookup`，strict `map_block` 只把 hole 映射为 `NotFound`；insert/remove 使用同一 tree。禁止恢复
  ext2 间接块映射或任何第二套 logical-block 路径。
- 每个 metadata block 读入必须先校验 checksum 再解释内容，写出必须经同一 seal 函数；checksum 失败
  返回 `InvalidFileSystem`，禁止跳过校验的读取路径。
- regular write 以 256 logical pages/1 MiB 为最大 transient batch，并复用 page-cache
  storage batch 的 capacity backoff；非对齐 1 MiB 可触及 257 个 filesystem pages，必须由
  实际 journal `NoSpace` 退避，禁止假定固定物理页数。
- 小于等于 4 KiB 的 regular write 使用未初始化 stack staging；大请求 heap reserve 与最终
  deallocation 必须位于 OFD position/write-sequence gate 外，失败时退回 4 KiB stack
  progress，不得新增 `ENOMEM`。copyin 通过 `UserInputStaging` 的 unsafe initialized-prefix
  publication 边界发布已由完整 copy adapter 初始化的 prefix，不做预清零；heap staging
  不得超过 1 MiB，且不形成 persistent state。

## Failure and cleanup

- rename/link/unlink/truncate 等 mutation 必须预留 journal/owner storage并提供完整 rollback；不能留下未索引 inode、错误 link count 或半提交 directory entry。
- open-unlinked inode 记入 orphan file slot；slot 内容只经 journal-aware metadata cache 读取，不保留
  内存索引，因此 transaction abort 丢弃 staged block 即完成回滚。final Drop 可在锁前只读 inode 状态
  作 admission，但 slot 必须在取得 filesystem mutation owner 后重新查找并 journal 清除。
- final inode Drop 不得等待 task-only mutation owner；owner 忙时只发布 filesystem 级合并
  retry bit，并由下一次 task-context mutation 在独立 transaction 中从 on-disk orphan file
  选择一个 Weak 已失效的 inode 回收。普通 mutation 只读该 bit；缺失延迟 owner 会令
  scheduler/deferred context 在锁竞争时 panic，直接跳过则会把空间永久泄漏到下次挂载。
- close/dup/CLOEXEC 在 fd-table lock 内只 detach；OFD drop、epoll/flock/record-lock consequence 在锁外执行。
- `SCM_RIGHTS` 传递 memfd 时只共享既有 OFD/inode identity；发送失败、接收 copyout 失败、
  connection EOF 与最后 fd close 沿通用 descriptor cleanup 释放引用及 mapping，不建立音频专用 fd 表。
- opened membership 的 register node 必须在 publication 前可失败预分配；OOM
  不得留下 raw pointer 或半发布 location key。rename 只回收并重用原节点，
  不在 inode mutation 提交后引入新的 allocation failure。
- `FileSystem::statistics` 是 fallible snapshot；ext4 取得 transaction owner 失败必须返回
  `OutOfMemory`，不得忽略 lock 结果后读取跨 superblock/group 的无锁中间状态。
- regular gather 必须按 user-page 边界 copy，使单个跨有效/坏页 iovec 仍可提交坏页前 prefix；backend short/error 后只推进 durable prefix。RLIMIT_FSIZE 在 non-append copyin 前裁剪，append 在 operation lock 内按 inode end 裁剪并保持 SIGXFSZ/EFBIG 与 position 语义。
- regular batching 的 blocking metric 使用 deterministic backend counters：对齐 1 MiB sequential
  write 必须只产生 1 个 journal transaction、至多 3 次 flush；257-page 非对齐形状必须证明
  capacity failure 无 publication 且退避后连续提交。wall time 仅作诊断，不作为 host gate。
- metadata cache 使用真实 ext image 与 counting block device 作 deterministic gate：16 次重复 lookup、
  cold-first getdents 与 warm extent mapping 测试窗口的 device read/allocation attempts 分别
  不得超过 `0/0`、`1/2`、`0/0`；固定 64-entry 线性 probe 的 CPU 成本有严格上界，当前不另设
  不稳定的 host wall-time benchmark。
- journal barrier 保持 `dirty-start + ordered data/descriptor/metadata durable → commit durable →
  home checkpoint durable` 三阶段；commit record 前必须存在 durability barrier，不能依赖同一 flush 内的
  device write ordering。只有 data 而没有 metadata 的提交直接写回 home 后单次 barrier。最后 clean
  marker 可延迟到下一 transaction 的首 barrier，crash 只会幂等 replay 已 durable home image。真实
  counting-device gate 要求单次 1 MiB batch 在 sync 后保持 1 transaction 且最多 3 flush；100 个小文件、
  256 次追加与 4 MiB 顺序写在 sync 后各只形成 1 个 transaction、至多 3 flush，设备写入不超过 data
  block 数加 64（改造前分别为 200/257/65 个 transaction，数据经 journal 写两次）；固定 64 data block truncate 只允许一次 allocation metadata
  materialization，gate 上限为 32 KiB metadata preparation。
- ext4 mapping structure gate 要求 extent lookup owner、lookup heap allocation、sparse delegation 与
  残留间接块标识分别为 `1/0/1/0`。
- ext4 崩溃矩阵在一个 running transaction 内混合 mkdir/create/write/truncate/rename/unlink，于 sync
  前与提交的每个 barrier 后崩溃：mount 必须成功，可见状态必须是完整旧状态或完整新状态（含 ordered
  data 内容），且 `e2fsck -fn` 零错误。
- ext4 conformance gate 在 fixture 副本上执行 htree 转换/两层 index、深层 extent、截断、跨目录
  rename、symlink、hard link 与 orphan crash recovery，最后要求 `e2fsck -fn` 零错误。
