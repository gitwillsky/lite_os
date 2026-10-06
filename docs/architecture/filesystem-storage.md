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
- devfs、devpts、procfs 与 sysfs 是 composition root 挂载的明确 adapter；它们不形成第二套 namespace 或对象状态。
- directory iteration 由 inode adapter 从 opaque cursor 直接推进：ext4 线性目录的 cursor 是下一 record byte
  offset，htree 目录的 cursor 是 hash 位置，内存型 adapter 使用 ordinal cookie；VFS 不物化完整目录，`getdents64` 只编码一个有界 batch。
- close、dup replacement、CLOEXEC 与 SCM receive 遵守 reserve/detach/publish 顺序，可能析构或通知的 consequence 在 fd-table lock 外执行。
- VFS `openat(O_CREAT)` 在 namespace mutation owner 内原子选择 existing winner 或 create commit；
  无 `O_EXCL` 的并发 append 不会因另一个 creator 先提交而误报 `EEXIST`。

## Known limits

- 当前持久存储范围是单个启动卷与固定 ext4/JBD2 profile。
- 没有通用 block scheduler 或多个可热插拔持久卷策略。
- 已返回的写入在 `fsync`/`sync` 前最多可能丢失 5 秒（与 Linux `commit=5` 一致）；块分配仍发生在
  `write` 时，没有 delayed allocation。
- ext4 磁盘保存纳秒时间戳与 crtime，但 VFS metadata 只投影非负秒数；`utimensat` 的纳秒部分与
  `statx` birth time 不对用户可见。
- orphan file 满时 open-unlinked 返回 `NoSpace`，不回退到 legacy orphan chain。
- 无 `largedir`：htree 最多两层 index，满后目录插入返回 `NoSpace`。htree readdir 在 batch 边界上，
  同一 hash position 的剩余 entry 会与停止点共享 cursor，可能被重放但不会遗漏。
- runtime 只更新 primary superblock/GDT；backup 副本由 e2fsck 修复，没有在线 resize。
