# 文件系统与存储当前架构

## 当前设计

- VFS opened entry 表达 pathname identity；OpenFileDescription 拥有 backend、offset、status flags 与 descriptor reference consequence；fd table 只拥有 slot 和 descriptor flags。
- ext2 revision 1 是当前可写 root filesystem。inode、directory mutation、link count、allocation 与 JBD2 metadata journal 在 filesystem owner 内闭合。
- page cache 唯一拥有 shared file page identity、dirty/writeback 状态和 reclaim cursor；VMA 与 filesystem 通过 shared-page seam 交互。
- devfs、devpts、procfs 与 sysfs 是 composition root 挂载的明确 adapter；它们不形成第二套 namespace 或对象状态。
- close、dup replacement、CLOEXEC 与 SCM receive 遵守 reserve/detach/publish 顺序，可能析构或通知的 consequence 在 fd-table lock 外执行。

## Known limits

- 当前持久存储范围是单个启动卷与已声明的 ext2/JBD2 子集。
- 没有通用 block scheduler、后台 writeback daemon 或多个可热插拔持久卷策略。
