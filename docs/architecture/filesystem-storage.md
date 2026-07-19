# 文件系统与存储当前架构

## 当前设计

- VFS opened entry 表达 pathname identity；`OpenedIndex` 以 exact ordered membership
  连接 register、rename/unlink 与 final Drop，路径解析不再按组件扫描全部
  live opened entries。index node 只持有 Weak；namespace mutation 在锁外 upgrade，
  且被替换 parent 与临时 strong pin 均在 index lock 外析构，避免 final Drop 递归取锁。
  OpenFileDescription 拥有 backend、offset、status flags 与 descriptor reference
  consequence；fd table 只拥有 slot 和 descriptor flags。
- ext2 revision 1 是当前可写 root filesystem。inode、directory mutation、link count、allocation 与 JBD2 metadata journal 在 filesystem owner 内闭合。
- ext2 filesystem owner 还持有按 filesystem block number 标识的 64-entry directory/indirect-pointer
  metadata cache；缓存只保存完整 block image，固定容量按 LRU reclaim，不形成 dentry 或 decoded-pointer
  第二身份。
- ext2 inode block mapping 由固定三层 `BlockPath` 唯一分类 logical block；lookup、sparse read 与
  allocation 共用同一路径，`PointerBlock` 是 cache-owned block image 的唯一 pointer decode seam。
- JBD2 active transaction 同时拥有 redo block set 与 allocation dirty-group bitset；block/inode alloc/free
  只标记 dirty group，commit 前一次性物化 primary superblock、受影响 GDT block 及其 sparse backups。
- page cache 唯一拥有 shared file page identity、dirty/writeback 状态和 reclaim cursor；VMA 与 filesystem 通过 shared-page seam 交互。
- devfs、devpts、procfs 与 sysfs 是 composition root 挂载的明确 adapter；它们不形成第二套 namespace 或对象状态。
- directory iteration 由 inode adapter 从 opaque cursor 直接推进：ext2 的 cursor 是下一 record byte
  offset，内存型 adapter 使用 ordinal cookie；VFS 不物化完整目录，`getdents64` 只编码一个有界 batch。
- close、dup replacement、CLOEXEC 与 SCM receive 遵守 reserve/detach/publish 顺序，可能析构或通知的 consequence 在 fd-table lock 外执行。
- VFS `openat(O_CREAT)` 在 namespace mutation owner 内原子选择 existing winner 或 create commit；
  无 `O_EXCL` 的并发 append 不会因另一个 creator 先提交而误报 `EEXIST`。

## Known limits

- 当前持久存储范围是单个启动卷与已声明的 ext2/JBD2 子集。
- 没有通用 block scheduler、后台 writeback daemon 或多个可热插拔持久卷策略。
