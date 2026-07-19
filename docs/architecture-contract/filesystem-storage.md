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
- ext2 owner 独占 inode/directory/link/allocation mutation；packed disk value 定义与字段保持
  `fs::ext2` parent-private，`fs::ext2::layout` 只封装定长 decode/encode 与 raw byte access，
  `fs::ext2::block_io` 封装 filesystem/device block 换算，`fs::ext2::inode` 独占 inode identity、
  block mapping 与 VFS projection；`Ext2FileSystem` 的 64-entry metadata block cache 独占 directory/
  indirect-pointer block identity 与 LRU reclaim；JBD2 journal 独占 transaction/commit/replay；page cache
  独占 cached page lifecycle。
- JBD2 active transaction 是 allocation dirty-group bitset 的唯一 owner；bitset 必须在 transaction
  publication 前按 group count fallible reserve，OOM 不得开始 mutation。alloc/free 只能标记 group，
  `MutationGuard::commit` 取走 dirty owner 后一次性生成 primary superblock、每个 dirty descriptor block
  及其 sparse-backup replicas；禁止每 block 重建全 GDT 或另设 pending-dirty 双轨。
- `RegularFileWrite` 的 write-sequence 与 operation gates 共同独占一次 syscall 的 position、append placement、storage transaction 和 resident-cache publication 顺序。
- VFS namespace mutation 与 ext2 live-state transaction 使用 `TaskMutex` 逻辑 owner；其内部
  spin gate 只发布 `Available/Held/Handoff(ticket)` 与预分配 waiter 链，logical guard 可以跨
  block I/O 和 task handoff，但不得保留 spin guard。竞争 waiter 必须进入 scheduler `Blocked`，
  unlock 在短锁内把 owner 直接交给最旧 ticket、锁外 exact wake；禁止恢复 spin/yield polling。
- page-cache `operation`/`write_sequence` 同样是可跨 cache fill、writeback 与 storage mutation
  保活的 task-only blocking owner；只有 resident page map 与全局 registry 的短临界区使用 spin。

## Interface

- filesystem 只通过 block seam 使用 driver，通过 shared-page seam 使用 memory，通过 unified backend façade 接入 pipe/socket/device。
- fd reservation 在 lookup/procfs/fork/close 前不可见；`recvmsg` 的 fd number 与全部关联 metadata
  copyout 成功后才能整批 publish，任一失败必须在 fd-table lock 外完成全部 reservation cleanup。
- OFD position 的推进只在对应 operation 已产生进度后发生；copyout 失败不得发布 `getdents64`
  position。`lseek` 结果必须能由 Linux signed `loff_t` 表示，不能把超出 `i64::MAX` 的值
  截断为负 syscall return。
- directory inode 只暴露 `read_directory(cursor, visitor)` 单轨 interface；`cursor` 是 adapter-owned
  opaque `d_off`，visitor Stop 不消费当前 entry。禁止恢复全量 `list()` 后按 OFD ordinal 截取。
  ext2 使用下一 record 的 byte offset，并只从 cursor 所在 block 开始；并发 mutation 令旧 cookie
  落入合并 record 时，在该 block 内向后对齐，因此已发布 entry 不重放、先前 block 不重读。
- `getdents64` 每批最多一次性预留用户容量与 64 KiB 上限的较小值；不得在 entry loop 内扩容。
  filesystem/编码/OOM/copyout 失败均不得发布候选 cursor，只有完整 copyout 后才在同一 OFD
  position transaction 提交。复杂度 gate 的 128-entry/4-entry-batch 模型要求零次全量 list、
  entry 物化不超过 128、output reserve 不超过 32，ext2 block read 不超过 block 数加 batch 数。
- pathname-backed OFD 必须保留 opened-entry identity；rename/unlink 不能把打开对象退化为字符串路径。
- opened index 的 exact node 只保存 `Weak<OpenedFile>`，不得增加 Arc cycle；mutation
  只在 index lock 内复制 exact key/Weak，再在锁外 upgrade。成功的临时 Arc 排除 final
  Drop，失败则不得解引用，并由已开始的 final Drop 精确撤销节点。dup/fork 仍共享同一
  OFD/OpenedFile，只有最后 Arc lifetime 结束时撤销节点。
- index lookup 只在锁内复制 exact key/Weak；upgrade pin、rename 替换出的旧 parent Arc
  必须在 index lock 外析构，否则最后 strong ref 会经 `OpenedFile::drop -> unregister`
  递归取得同一锁并在单 CPU 死锁。
- packed disk layout、journal block、device adapter 与 syscall UAPI 不得穿过 VFS seam。
- directory 与 indirect-pointer 读取只能经 filesystem-owned metadata block seam；journal stage 成功后必须
  在释放 journal lock 前更新或失效同 block cache identity，commit/home write 保持新 image，abort 与
  commit failure 必须失效 staged identity，truncate/free 必须在 block 可重用前失效旧 identity。cache
  miss admission 的 allocation 失败不得发布 partial entry；generation 改变时不得发布过期 miss。
- journal commit 必须把 `Journal` 与 immutable staged write view 从短 spin owner 中 loan 出来；
  descriptor/data flush、home checkpoint 与 clean-state write 全部在 owner lock 外执行。commit
  期间 reader 只短暂取得 staged view，cache miss 可继续访问 home device；禁止把 block I/O
  重新放回 journal spin guard。commit failure 必须清空 metadata cache 并把 journal 标记为
  fail-stop，后续 mutation 不得另走无 journal 兼容路径。
- ext2 inode mutation 只能使用锁外 `InodeMutation` working copy；普通 inode spin guard 只允许
  取得或发布一个完整 `Ext2InodeDisk` snapshot，不得跨 journal/block I/O。working copy 的类型
  lifetime 必须借用 `MutationGuard`，所以全部 live inode 发布必然发生在 commit 消费并释放
  filesystem mutation owner 之前；禁止恢复返回 `MutexGuard` 或依赖函数退出后的延迟发布。
- mount consistency scan 每轮只在短 spin 临界区复制一个 group descriptor，再在锁外读取
  block/inode bitmap；即使 filesystem 尚未发布，也不得让普通 spin guard 跨 DriverIo sleep。
- logical-block mapping 只能由 allocation-free `BlockPath` 分类 direct/single/double/triple 路径；strict
  lookup 只把 sparse traversal 的零结果映射为 `NotFound`，allocation 使用同一 path。pointer metadata
  只能通过 `PointerBlock` load/decode，禁止恢复 `map_block`/`map_block_sparse` 或 full/single-pointer
  各自读取 block 的双轨实现。
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
- close/dup/CLOEXEC 在 fd-table lock 内只 detach；OFD drop、epoll/flock/record-lock consequence 在锁外执行。
- opened membership 的 register node 必须在 publication 前可失败预分配；OOM
  不得留下 raw pointer 或半发布 location key。rename 只回收并重用原节点，
  不在 inode mutation 提交后引入新的 allocation failure。
- `FileSystem::statistics` 是 fallible snapshot；ext2 取得 transaction owner 失败必须返回
  `OutOfMemory`，不得忽略 lock 结果后读取跨 superblock/group 的无锁中间状态。
- regular gather 必须按 user-page 边界 copy，使单个跨有效/坏页 iovec 仍可提交坏页前 prefix；backend short/error 后只推进 durable prefix。RLIMIT_FSIZE 在 non-append copyin 前裁剪，append 在 operation lock 内按 inode end 裁剪并保持 SIGXFSZ/EFBIG 与 position 语义。
- regular batching 的 blocking metric 使用 deterministic backend counters：对齐 1 MiB sequential
  write 必须只产生 1 个 journal transaction、至多 3 次 flush；257-page 非对齐形状必须证明
  capacity failure 无 publication 且退避后连续提交。wall time 仅作诊断，不作为 host gate。
- metadata cache 使用真实 ext image 与 counting block device 作 deterministic gate：16 次重复 lookup、
  cold-first getdents 与 warm single-indirect mapping 测试窗口的 device read/allocation attempts 分别
  不得超过 `0/0`、`1/2`、`0/0`；固定 64-entry 线性 probe 的 CPU 成本有严格上界，当前不另设
  不稳定的 host wall-time benchmark。
- journal barrier 保持 `dirty-start durable → descriptor/data/commit durable → home checkpoint durable`
  三阶段；最后 clean marker 可延迟到下一 transaction 的首 barrier，crash 只会幂等 replay 已 durable
  home image。真实 counting-device gate 要求单次 1 MiB batch 保持 1 transaction 且最多
  3 flush；固定 64 data block truncate（另含一个 indirect block）只允许一次 allocation metadata
  materialization，当前单-group fixture 上限为 8 KiB metadata preparation。
- ext2 mapping structure gate 固定覆盖 direct/single/double/triple 的 96-block toy address space，并要求
  path classifier、metadata loader、heap path allocation、strict/sparse traversal 成本为 `1/1/0/1`。
