# 文件系统接口、事务与退休契约

## Interface

- `ProcSource::snapshot(ProcSnapshotScope, &PidNamespace)` 只投影既有 task/memory/CPU owner。
  System 用于 meminfo/vmstat/buddyinfo/stat/loadavg/uptime/net 和 tmpfs 容量，保留全局字段、
  scheduler task 数量，但不获取 mm、遍历 resident 或 pin 任意 TCB；Process(TGID) 用于
  指定 stable TGID 的 status/statm/thread，只沿 graph 精确 key 采集目标线程组；scope 是读取需求，
  不是另一份 registry/cache。root getdents 使用 process_identities，沿 mounted PIDNS 的实际
  编号 index 枚举 TGID，不构造资源快照；精确 root lookup 用 task_identity，在 PID index 和
  graph 上直接查找 leader/zombie/nonleader identity。
  `/proc/TID` 与 `/proc/TGID/task/TID` 的目录父入口由 ProcThreadDirectory 区分，内容使用同一
  Thread projection；root getdents 只列 TGID。TaskExe/TaskFd 节点按实际 TID 与 fd 完整 key
  admission，Thread fd/exe 不借 leader 资源。FileTableOwner 独占 fd snapshot 的生成，
  Thread/Process 只沿各自真实 outer handle 投影；已退出 leader 返回空，live nonleader 使用
  自己的 table。upper ProcSource::thread_file_descriptors 在 graph 外按实际 target 凭据
  完成 FSCREDS 授权；OOM 与 missing 分开透传，link_target 保留实际 opened identity。
  Process mm/files/state/scheduling 从原 leader owner 读取，退出后不回退到 worker；
  Thread status/stat/statm 使用自身 has_mm/FDSize。common 页数由 task owner 扫描一次，
  无 mm 的 stat/statm 零、status 无 Vm 字段；text codec 不再自行推断 mm presence。

- filesystem 只通过 block seam 使用 driver，通过 shared-page seam 使用 memory，通过 unified backend façade 接入 pipe/socket/device。
- `memfd_create` 只发布 anonymous regular OFD；`MFD_ALLOW_SEALING`、`F_ADD_SEALS`、
  `F_GET_SEALS`、`ftruncate` 与 `MAP_SHARED` 沿既有 fd/inode seam 工作（内容在 `MemoryFile`，经
  `Inode::data_backing` 分派，不经 page cache），不注册 pathname 或引入私有 shared-memory ABI。当前 seal 支持 `SEAL|SHRINK|GROW|WRITE|FUTURE_WRITE`（写映射 lease 见文件系统存储契约），未支持的 hugetlb
  语义明确返回错误。F_ADD_SEALS 先判 Memory backing，再判 OFD writable，最后校验 mask；RO memfd 的零/未知 mask 均 EPERM。add_seals 与 truncate/write 共用 write_sequence；truncate 先 check seal，再 prepare metadata guard、执行可失败 pages
  mutation，成功后无失败提交 killpriv。所有 pages/metadata guards 在 MM invalidation 前释放；seal/OOM/I/O 拒绝不清 set-ID、不改 size/time。
- `openat(O_CREAT)` 的 final lookup 与 create 必须在同一次 VFS 父目录锁独占持有
  内完成：存在且无 `O_EXCL` 时打开 winner，存在且有 `O_EXCL` 时返回 `EEXIST`，不存在时创建。
  禁止锁外先 lookup 再调用独立 create；该双阶段会让并发普通 append 错误收到 `EEXIST`。
- file 的 `PreparedFileDescription` 是唯一 OFD allocation protocol：Arc<MaybeUninit<OFD>>
  在 pathname/create/backend 前预分配，只有消费式 initialize 完整写入后才进入 OFD Drop lifecycle。
  Token 不 clone、不创建 Weak；失败只释放未初始化 allocation，不发送伪 close。所有匿名与 named
  constructors 共用该 protocol，不保留 Arc::try_new(Self) fallback，也不增加 opened shadow flag。
  PreparedNamedFile 在 backend 打开前统一取得 regular/bdev write lease；Storage/Device
  使用同一无分配 finish，proc regular operations 不能绕过挂载写者。backend 成功后 IN_OPEN 先于 truncate；truncate 拒绝仍发送配对 close，O_TRUNC 始终要求
  write permission（含 O_RDONLY），但 OFD access mode 不因此改变。
- `openat` 在 pathname lookup/create、device/FIFO admission、truncate 前由原 FileTable
  预留不可见 fd；`OpenFdReservation` pin 原表，成功无分配安装 OFD，失败 Drop 只撤销原表 slot。
  未准备 OFD 的 reservation 与 SCM 的已准备 OFD reservation 共用 descriptor publication owner；
  slot 仍为 16 bytes，禁止另造 bitmap/table 或用假 OFD 占位。fd-limit/backing OOM 不产生文件副作用。
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
- 每个 OFD 必须保留 opened-entry identity；canonical Dentry 用独占 Box<Mutex<Location>> 保存关系，anonymous dentry 只持 immutable display Vec，不注册 DentryIndex、不投影 parent/root、不触发 unlinked
  eviction。标签在创建时可失败准备，proc enumeration 只 clone identity，readlink 才复制标签；不能按标签重开。
- `Inode::open_backend` 只返回 Storage/Device 语义，不依赖 file/task；权限、errno 与 user-copy 由 syscall 处理。mmap/positioned I/O/seek/sendfile 按实际 backend admission，不能因存在 inode 就允许普通文件操作。
- pseudo Metadata 保存真实 creator FSUID/FSGID/mode；pipe 两端共享 FIFO0600 inode，socket 为 socket0777且只保留 scalar object-ID，不 strong-pin Socket。anon_inodefs singleton 内部 kind File、getattr 无 S_IFMT、setattr EOPNOTSUPP、reopen ENXIO、S_PRIVATE
  抑制 fsnotify；每个 dentry 标签独立。MemFile 为 regular0777/nlink0，同一 inode 重开取得独立 position/status/CLOEXEC，共享内容/seals。
- `Inode::pseudo_statistics` 与 VFS mounted statistics 是同一 path/fd statfs projection，读取同一真实内部 filesystem；sockfs/pipefs/anon_inodefs/hidden shmem 身份不同。
- 通用 OFD 成功 I/O 与最后 Drop 按真实 opened inode 发 watch 通知，设备/FIFO/pipe 也适用；没有 watch 时不查 metadata、不 clone Arc，private singleton 在 inode hook 抑制。
- DentryIndex/MountIndex 只保存 exact Weak；短锁内复制 key/Weak，upgrade 与 consequential Drop 锁外。
  Dentry 最终 Drop 精确撤销其 node；MountView 的预留退休 payload 经 VFS gate 精确撤销。
  rename 退下的 parent、filesystem、device 与 endpoint 均不得在 registry spin 内 final Drop。
- packed disk layout、journal block、device adapter 与 syscall UAPI 不得穿过 VFS seam。
- directory、htree、extent node 与 orphan block 读取只能经 filesystem-owned metadata block seam；journal stage 成功后必须
  在释放 journal lock 前更新或失效同 block cache identity；home checkpoint 只持久化 running preimage，
  不得覆盖 active overlay 的逻辑 cache publication。abort 与
  commit failure 必须失效 staged identity，truncate/free 必须在 block 可重用前失效旧 identity。cache
  miss admission 的 allocation 失败不得发布 partial entry；generation 改变时必须在返回 caller 前
  使用当前 cached image 或复用未发布 buffer 重读，不能仅拒绝 admission 后返回撕裂的 home image。
  通用 `read_fs_block` 对 journal miss→home I/O 同样重验 epoch，直接 inode/file block
  caller 不依赖 metadata-cache admission 才获得完整 block snapshot。
- journal commit 必须把 `Journal` 与 running/active immutable staged views 从短 spin owner 中 loan 出来；
  descriptor/data flush、home checkpoint 与 clean-state write 全部在 owner lock 外执行。commit
  期间 reader 保持 active → running → home 的唯一新旧顺序。两个 Arc control block 在状态转换前
  fallible 分配，节点与字节只移动；active view 在 commit/drop 时按 identity 原样交回 handle，
  reader 的 Arc 不得逃出 journal guard。cache miss 可继续访问 home device；禁止把 block I/O
  重新放回 journal spin guard。commit failure 必须清空 metadata cache 并把 journal 标记为
  fail-stop，后续 mutation 不得另走无 journal 兼容路径；`fsync`/`execution::sync` 报告该提交失败的 EIO。
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
- retry bit 只在 mutation owner 内消费与重新发布；task-context sync 沿同一回收路径逐个排空已失去
  live Weak 的延迟 orphan（每个 transaction 仍只回收一个），取得 owner 后重验待办再提交。缺失时
  `rm; sync` 后磁盘留下 nlink 0、dtime 0、仍占 bitmap 与 orphan 槽位的 inode；仍被打开或映射的 orphan 保留。
- close/dup/CLOEXEC 在 fd-table lock 内只 detach；OFD drop、epoll/flock/record-lock consequence 在锁外执行。
- `SCM_RIGHTS` 传递 memfd 时只共享既有 OFD/inode identity；发送失败、接收 copyout 失败、
  connection EOF 与最后 fd close 沿通用 descriptor cleanup 释放引用及 mapping，不建立音频专用 fd 表。
- opened membership 的 register node 必须在 publication 前可失败预分配；OOM
  不得留下 raw pointer 或半发布 location key。rename 只回收并重用原节点，
  不在 inode mutation 提交后引入新的 allocation failure。
- `FileSystem::statistics` 是 fallible snapshot；ext4 取得 transaction owner 失败必须返回
  `OutOfMemory`，不得忽略 lock 结果后读取跨 superblock/group 的无锁中间状态。
- regular gather 必须按 user-page 边界 copy，使单个跨有效/坏页 iovec 仍可提交坏页前 prefix；backend short/error 后只推进 durable prefix。RLIMIT_FSIZE 在copyin前裁剪，append在write sequence内按inode end预检并在operation lock内复验并保持
  SIGXFSZ/EFBIG 与 position 语义。
- regular batching 的 blocking metric 使用 deterministic backend counters：对齐 1 MiB sequential
  write 必须只产生 1 个 journal transaction、至多 3 次 flush；257-page 非对齐形状必须证明
  capacity failure 无 publication 且退避后连续提交。wall time 仅作诊断，不作为 host gate。
- 物理 claim 的 raw-read cache policy projection 使用生产 owner 的 blocking benchmark，覆盖一/九个
  extent，沿用统一 200 ns/op 上限；节点 prepare/alias eviction 属于 open/mount 路径，
  不做热路径分配。真实 claim/registry/RegularFile host 回归验证 foreign writer 后不重建
  alias cache、准备中 claim 变更拒绝发布，以及 registry allocation 的重入/OOM；仅 frame、
  inode storage 与 scheduler adapter 为 host seam，不声称覆盖 VirtIO 或全局 reclaimer 实现。metadata coherency 的暂停设备回归覆盖 active 强制 running commit
  的冷/热读取，以及 journal miss 后跨 checkpoint 的撕裂 home read。
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

- block swap adapter 在同一 physical extent claim 下直接执行4KiB block I/O，绕过 page cache
  和 byte-range scratch；activation 失败由 lease Drop 回滚，成功 swapoff 显式 close 后才返回。
  现有 snapshot/raw read 只访问已提交块内容，不能建立独立 swap alias cache。

- `fs::swap::SwapDisk` 是 block/file 共用的 immutable page→device extent I/O adapter；
  `SwapSource` 保留 O_RDWR mount writer lease 与原 OpenedFile，成功 close 才交还。
  VFS `WriteOpen` 是 OFD/swap 共用的 mount admission RAII；file 层只额外持 bdev writer claim。
- ext4 canonical `LiveInode` 同锁持有 disk image 与 Free/Preparing/Active swap phase；
  journal working-copy/abort 仅更新 image，不能覆盖 lease。普通 data/layout mutation 在 FS
  mutation 域拒绝非 Free；只有 `writeback_storage_batch` 可排空 Preparing，Active 禁 dirty write。
  unlink/rename source/target 在修改前按同一 inode phase 拒绝；VFS namespace 域复查原 entry
  尚 linked 后冻结，mount writer 与 remount,ro 同锁准入，OpenedFile lifetime pin umount。
- 激活顺序为 write_sequence→operation→namespace/FS freeze，释放 operation/FS 后维护 mm；
  fault 既有 mm→operation 锁序不能反转。统一 shared leaf projection 保留 VMA W，但冻结时屏蔽
  PTE W；fork/fault/mprotect 共用，post-fence claim release 按实际 leaf，不重读变化的 gate。
  `MappedFile.may_write` 只保存原 fd 的 VM_MAYWRITE，不由现在的 fd 或 swap phase 重算。
- cache writer 最后一次 release 在实际 page CAS 后 signal 单 CachedFile 预备 Arc<TaskEvent>；
  write_sequence 排除第二 activation waiter。generation-aware mm revocation 后按同一 pages
  中真实 writer bits 等待已失 Weak identity 的退出字段，再 flush dirty/checkpoint/barrier。
  cache operation 内读原逻辑 header；最终 export 接受 allocated unwritten extent，拒绝 hole。
  当前唯一4KiB ext4 profile无需物理裁剪/页重新编号；header 与 swap page I/O seam 独立。
  lease Option take 幂等解除 phase，失败 Box/Arc preparation 与显式 close 都锁外析构，
  旧 snapshot Drop 不得解除后来取得的 lease。

POSIX byte-range lock 的 owner identity 是稳定 FileTableOwner Arc value 地址，l_pid 独立
保存请求者 TGID。最后 table Drop 在地址释放前清除全部锁，避免地址复用 ABA；lock entries
不强引用 table，不能形成资源环。CLONE_FILES 跨 Process 共享同一 owner，unshare/fork
复制 OFD entries 后取得新 owner 且不继承 POSIX 锁；flock 仍归 OFD。任意 fd/CLOEXEC close
清除此 table 在 inode 上的全部 POSIX 锁；shared exec 的新表不能清掉旧共享表的锁。
相同类型扩展按固定 Linux 保留第一个既有 lock 的 l_pid，不从 owner identity 反推 PID。
POSIX 加锁成功（非 unlock）后必须在同一 FileTable guard 下复查原 fd 仍指向捕获的
OFD；共享表的 close/dup2 可在阻塞等待期间发生。失配时锁外清除本表在原 inode 上的全部
锁并返回 EBADF；不能因持有旧 OFD Arc 就把已经关闭的 descriptor 当成仍有效。

`sendfile::regular_transfer` 唯一拥有kernel buffer transfer、killpriv时点与durable offset推进；
descriptor/offset UAPI留在sendfile入口。host生产transfer用deterministic input EOF/EIO/output
failure seam验证order，Ext4/tmpfs真实inode权限在owner测试与两ISA guest中验证。

## 执行文件与访问时间

- `InodeWriteAccess` 属于 canonical ext4/TmpInode/MemFile，而非 mount 或 pathname；signed i_writecount
  正数表示 writer、负数表示 execution deny-write，两种准入在同一短锁互斥，冲突返回 ETXTBSY。
  `ContentWrite` 同时持 inode access 与 mount writer；final fput 先消费 inode access，
  再由最后 dentry 逐出 unlinked cache/释放 inode，最终才归还 mount writer。
  named OFD 在 backend 副作用前取得；O_RDONLY|O_TRUNC 取得短期同类 lease，别名不能绕过。
- exec 初次 DAC 检查后取得 deny-write，再同 inode owner 重新捕获 uid/gid/mode 并复验 execute，
  禁止把 writer 清过 set-ID 的旧 snapshot 当凭据。mnt_may_suid 来自 actual caller namespace membership、
  nosuid 与 superblock UserNS ancestry；foreign mount/anonymous hidden mount 不授予 set-ID。
- 主程序 actual ExecutableOpenLease 只由 MemorySet executable_owner 生命周期保活；fork/CLONE_VM
  继承同一 Arc，munmap 所有 LOAD 也不提前释放 deny-write。解释器租约只覆盖构造期，VMA 只持随机
  读取 source。Process/LoadedExecutable 不复制主程序 path owner；proc exe 使用 exact Thread 的 mm，
  无 mm/原 leader 退出返回不存在，不改读 sibling 或延长 zombie 的 inode/mount lifetime。
- actual OpenedFile 解释 NOATIME/NODIRATIME/RELATIME/strict policy；statfs 与 proc options 同一 flags。
  新 mount 默认 relatime；remount 未指定 atime policy 保留原策略；跨 UserNS clone 锁定完整 atime mask。
  读取不在 ext4 read_storage/page-cache miss 内改时间。page-cache/shmem 非零 read transaction 的 borrowed
  ReadAccess 在 EOF、I/O error、copyout EFAULT 等返回同样触碰，zero/admission failure 不触碰；
  IN_ACCESS 仍独立按实际数据交付投递。目录、symlink、mmap、exec 以实际 path 触碰。
  update_time 只更改 atime、不改 ctime/mtime，复查同一 tuple 的 relatime 条件；clock 在短锁外读取。
  mount writer 使用 try admission，RO/reconfigure 或 metadata/I/O 失败跳过 atime，不破坏成功读取。
- TmpFileSystem 独占 immutable creation root options 与 live StorageBudget。remount 校验 mode/uid/gid
  但只改变容量，不能重写 live root inode attrs/ctime。show_options 读取实际预算与 creation options，
  不从 root chmod/chown 或原始 mount 请求字符串猜配置；UID/GID 以 initial UserNS kuid 展示。
- 冷 mount/clone/exec owner 分配与热 read flags/try admission 的成本必须在现有成本门禁 中评估；
  旧版本验证结果不能作为新实现通过证据，提交须执行当前产品门禁。

- ProcLookupCache 只由实际目录 inode 拥有、只保留向下 child Arc；`.`/`..` 不进入缓存。canonical inode/属性/号仍是 ProcIdentities/ProcInode，缓存不生成另一份事实。
  目录缓存窗口为 MAX_GETDENTS_BATCH_BYTES / size_of(DirectoryEntry)，仅约束可回收 pins，
  不约束真实目录。超额在短锁内摘除最早签发 entry，完整 child/node Drop 在锁外；打开 inode
  继续由 live alias pin，父目录退休释放整个向下子树，不存在 Arc cycle 或无限进程历史表。
  对照 Linux v7.1 fs/proc/base.c::proc_fill_cache，readdir 与 lookup/lstat 使用同一 cached inode；
  队列淘汰等价于 dcache eviction，不承诺被回收且无 live alias 的 inode 编号永久不变。
  benchmark 决策：cache hit 为 AVL O(log n)、无分配；冷 admission 增加一次锁外 AVL node，
  满窗口淘汰有界 O(window) 扫描。集中 host/guest 验证实际重枚举、父入口与退休，并保留原性能门禁。
- fd/ns 子目录持 ProcTaskDirectory 的实际 parent context；Process、直接 TID、task/TID 三种
  路径各自保留正确父 inode，task/TGID 不因 TGID==TID 被改成 ProcessDir。全部内容 producer
  仍只读取相同实际 stable TID。cmdline 改由 thread_arguments 读取该 Thread 的 MM；原 leader
  退休为空，live sibling 仍读自己的 MM，argument range 不可读为空而非伪造 Task 消失。
  graph_snapshot_rows 只 pin目标 Process 的详细关系与 group/session identities；System 不创建
  Process rows。全局 runnable 数及到期 load-average 样本直接读取 ProcessorTopology 已有的
  Ready/Running membership，包含内核线程，排除 idle、未激活 New 与 stale queue token；采样
  与 Linux nr_running 一样不是跨 CPU 原子快照。total_tasks/sysinfo 当前按实际用户 TID index
  的 O(1) len 投影，内核线程尚未进入用户 graph，不能宣称它是完整 Linux nr_threads。
  benchmark 决策：系统采集不随进程/线程数量增长，不分配 TCB 快照；单进程采集为
  O(log P + T_target)，只有目标线程数组在 graph 外 fallibly 预留，竞态容量增长重试。
  process_graph_cost 对固定 P=1024/T=8 约束无关 Process 访问为零，并含退化扫描 mutation；
  最终统一运行该门禁及已有 proc guest/性能验证。

- proc sysctl inode 的 User/PID/IPC target 改持 actual namespace Weak；目录 child cache 不延长
  namespace 配额生命周期。lookup key 仍含完整 actual scope ID，成功 open backend 才升级并
  pin 同一真实 target，dup/SCM_RIGHTS/setns 保留目标。Weak 已退休则 lookup/open 为 ENOENT，
  不重新解释旧 scope 成当前 namespace。PID/user/message sysctl 的 read/write 仍由各 actual
  owner 负责，inode cache 只保留 inode号、属性与向下生命周期，不复制限制或 namespace state。

- `OpenedFile::watch_context` 的 fs 内接口返回同一实际 dentry snapshot 的 parent/name/unlinked，供 inotify
  实现 IN_EXCL_UNLINK；`is_unlinked` 对 fs owner 可见，作为 snapshot allocation 失败时的实际状态查询。
  `DeviceError::ReadFault` 是 Linux record read 的 fatal copy fault，syscall 的统一 progress projection 保留 EFAULT；
  其他设备错误仍返回已经完成的字节数。PreparedDeviceDescription 在创建 per-open backend 前准备 OFD/inode storage。

- file::signalfd 拥有 SignalFd.mask、SignalWaitSource.generation 与 FileReadinessContext seam；不依赖 Task。OFD 的 poll_events/readiness_generation/readiness_sources 显式接收上层上下文，epoll ready projection 传递同一上下文。SourceNotificationContext 仅表
  示无用户 reader 的 deferred 阶段，signalfd/嵌套 epoll 在该阶段不读取 pending，保守入 ready 后交实际 caller 复查。signalfd 创建在 backend 发布前准备 OFD/inode，全量失败归还已有准备；最后关闭没有第二份 signal queue 可回收。

Sequential read 不再以全局 count==0 捷径跳过 OFD 类型操作；eventfd/timerfd/signalfd、epoll 和 directory 必须保留自己的 native 最小记录或不支持 read 错误，socket 的空 VFS iterator 由实际 socket façade 按 sock_read_iter 返回零。新增匿名
文件 guest 对前三类 descriptor 的零 read 错误检查。

- pseudo mount 的 root 只持规范 filesystem 类型与创建时间，不保活 endpoint/PID/namespace；
  每个域的两个 Once 分别保活实际 superblock/root 和实际 view，view 构造 OOM 后借既有已登记 FS
  重试，不能重签相同保留 FSID。域间不共享 mount identity，域内所有 OFD/magic reopen/bind 借
  同一 owner。类型显式传入，诊断 label 不参与 dispatch；cold 首次构造用实际 owner OOM/alias
  回归覆盖，warm open 仅新增现有 path shell 的 mount Arc pin，成本纳入最终 OFD benchmark。

- O_CREAT 与 lookup 共用唯一 PathLookup walk：create intent 只允许最终 normal component 缺失，
  在最终父目录独占持有内将展开后的实际 parent/name 交给 create_child 回调并返回 Created；禁止因
  跟随链接后的 ENOENT 重走原始路径。普通 lookup 的 ENOENT 不额外分配，create normal 末项的
  trailing slash 在父搜索后返回 EISDIR，缺失中间目录保留 ENOENT。新建结果只在一次 open 中
  传递，FMODE_CREATED 按 parent 授权免除新 inode mode 检查/O_TRUNC，不作为 inode 缓存状态。
  O_CREAT|O_DIRECTORY 在普通 open flags 校验中为 EINVAL；O_PATH 先剥除 create/access flags。
  新分支属于冷 lookup；已有 walk/创建与 guest 链式链接回归覆盖，保留全部既有性能门槛。
- 双 fd I/O 在同一 table guard 捕获实际 OFD 后释放表锁、检查 O_PATH 并执行 operation；
  sendfile/epoll 共用此权限入口，SCM_RIGHTS 与 dup 的 raw capability 保持各自 native policy。
  新的表锁外执行消除 epoll mutation 的分配/释放嵌套；最终 FD/OFD benchmark 验证成本。

- 纯 root `/` 的 walk 不对最终目录额外检查 X，实际 component 的 parent search 仍逐项执行；
  O_PATH/read-only directory 与 chdir/fchdir 各执行自己的 native capability 授权。空路径在
  walk 返回 ENOENT，AT_EMPTY_PATH 的 raw descriptor projection 留在 syscall；实际 walk
  search 调用计数与无 X 的 chroot/UID actor 回归已补源码。


proc 网络文件的源码归属已根修：`/proc/net` 是标准 `self/net` symlink，
`/proc/PID/net` 与 `/proc/PID/task/TID/net` 只保存 actual target selector，每次 lookup/getdents
在 task 的真实 nsproxy 下捕获 NET；退休目标 lookup 为 ENOENT，getdents 为 EINVAL。
NetDev/NetRoute inode 的 canonical key 含 actual NET identity，只持 Weak，不作为全局
registration entry，也不保留 target Task。实际 open backend 消费一次 child Weak，失败 ENXIO；
成功的 Network variant 只持 ProcSource、具体 table 与 Arc NET，dup/SCM/Task setns/exit 不改变
读取对象，最后 OFD release 释放 pin。两种网络文件无 write operation，write-mode open EACCES。

ProcSource::network_snapshot 接受显式 captured NET，protocol owner 提供全部实际接口的
锁外配置/counter快照；ProcSnapshot 删除 network 字段，System/Process 读取不再取得网络锁。
专用 procfs::networking 用 raw bytes 输出原生接口名及真实 transmitted_dropped，不能固定
eth0/首设备或在读时从当前 Task 重解析。route formatter 消费 captured NET 的实际 FIB
route_snapshot，不从接口配置重建路由。冷读取的 Vec/proc text 分配均 fallible，无新增 global
或锁；实际 open-pin/weak gone/native layout 与名称/drop 计数回归覆盖该生命周期。

sysfs `/sys/class/net` 由 `sysfs_net` 投影：`NetLinkSource`（task 层 `KernelProcSource` 实现，经
`MountEnvironment::net_links` 注入）提供挂载者 NET 与实际接口快照，`NetView` 在 mount 时只弱引用该 NET，
NET 销毁后列表为空；每次 lookup/read 重新取快照，以 ifindex 为节点身份，无缓存、global 或锁。
`InterfaceSnapshot::driver` 来自 `NetworkDevice::driver`（loopback 无驱动），`device/uevent` 输出 `DRIVER=`；
`flags` 复用 `InterfaceSnapshot::flags`，与 `SIOCGIFFLAGS`、rtnetlink 同源。`SysFileSystem::new` 只由 sysfs 类型调用。

Unlink/rename 提交后先释放 namespace guard 与临时 victim inode pin，再归还实际 path。
只有 Dentry final Drop 调用 evict_if_unlinked；page-cache registry 不接受 caller 自报 holder
数量。已有 FD/mmap 保留真实 path/cache owner，dirty 内容不会被提前丢弃；最后持有者释放
才回收。ContentWrite 的 Option 存放唯一可消费 inode access capability，无镜像生命周期。
release_inode_access 的唯一 caller 是 file::OpenFileDescription final Drop；无分配、无新锁。

最后 dput 摘除 unlinked cache 的 registry entry 不等待临时 I/O 的 CachedFile Arc。
已有 I/O 保活同一 inode 与页，最后 I/O 结束自然释放，避免错过唯一 dput 后永久保留 orphan。
