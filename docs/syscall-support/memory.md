# Virtual memory syscall

Linux VMA/映射政策由 `system/linuxd/src/memory` 拥有，native VMO/VMAR 只提供机制。

| Number | Syscall | Status | 当前范围 |
|---:|---|---|---|
| 214 | `brk` | Complete | program break 与匿名 VMA 统一 owner |
| 215 | `munmap` | Complete | VMA split、共享写回、native 翻译撤销与清理 |
| 216 | `mremap` | Partial | 私有/共享匿名、文件及 device 映射缩小、增长、MAYMOVE/FIXED/DONTUNMAP、old_size=0 共享别名；stack/loader System VMA 返回 EFAULT |
| 222 | `mmap` | Partial | anonymous、regular file、memfd、ALSA PCM；private/shared/fixed；populate/noreserve/stack 为 advisory，按需补页 |
| 224 | `swapon` | Unsupported | ENOSYS，无交换设备或页迁移机制 |
| 225 | `swapoff` | Unsupported | ENOSYS，无交换设备或页迁移机制 |
| 226 | `mprotect` | Complete | protection combinations、VMA split 与最大权限约束 |
| 227 | `msync` | Partial | 共享文件映射同步写回，无通用脏页压力控制 |
| 233 | `madvise` | Partial | 下述映射 flags、discard 与访问提示；无后台回收 |

## 映射与寿命

`MAP_PRIVATE` 文件读页来自 canonical pager VMO，写入使用 snapshot COW。fork 生成 VMO snapshot，
子分支退出后内核合并只剩一个存活子分支的 hidden parent；truncate 退休范围外页与映射。
`mremap` 迁移 VMA owner/backing，不复制物理页；旧翻译同步撤销后才退休资源。
共享匿名映射增长只扩展映射，不能扩展原 backing；新增页访问得到 SIGBUS。共享文件增长至 EOF 外同样 SIGBUS，device 映射不能增长。
`old_size == 0` 只允许共享源，且必须 MAYMOVE；缺失时按固定 Linux v7.1 返回 ENOMEM。
普通源须由单个 VMA 覆盖；stack 大小由 stack owner 管理，不可 remap。

native `ALLOW_FAULTS` 允许映射覆盖尚无 backing 的范围，实际访问再失败；Linuxd 负责把该故障转换成 SIGBUS。
此服务 flag 不对 Linux 应用暴露新的 ABI。私有匿名 MAP_GROWSDOWN 按相邻 VMA 与地址限制扩展。

## Advice 与当前边界

- NORMAL/RANDOM/SEQUENTIAL/WILLNEED 是 advisory，不预取、不建立后台回收状态。
- DONTFORK/DOFORK、WIPEONFORK/KEEPONFORK、DONTDUMP/DODUMP 保存在实际 VMA；WIPEONFORK 只接受私有匿名映射。
- DONTNEED/FREE 当前对私有页同步 decommit；FREE 不提供延迟回收的内容保留期。共享对象保持内容；没有物理 swap 或通用压力回收。
- 没有 commit accounting、NUMA、userfaultfd、应用 huge page 或通用脏页压力政策。
- 文件内容缓存由其文件服务拥有，每个 inode 一个 pager VMO；ext4 文件页在 ext4 服务中，linuxd 本地文件缓存与内存文件使用自己的 pager 工作池。NOWAIT 读当前仍可能阻塞，不声明非阻塞磁盘读取。
- `/proc/swaps` 不存在，Swap 字段为 0。meminfo 的 AnonPages（私有匿名与私有文件映射的 COW 页）、Mapped（共享映射）由 linuxd 按全部存活进程的 VMA
  后备对象去重求和；Shmem 为共享匿名/已附加 SysV 段的驻留页加 tmpfs/memfd 已计费页（未附加的 SysV 段不计）；Slab/KernelStack/PageTables 取内核
  `MEMORY_STATS`（内核堆、已映射内核栈、CPU 与设备页表）。Cached 来自内核全系统 pager 驻留页，包含独立 ext4 服务与 tmpfs/memfd；
  Dirty、Writeback 和 vmstat 的 nr_dirty 只含其中有存储写回目标的页，不把内存文件的保留内容当成待落盘页；MemAvailable = min(MemTotal, MemFree + 干净且未固定的 pager 页)，不把脏页、回写中或固定页算作可用。vmstat 的 `pgscan_direct`/`pgsteal_direct`
  为累计淘汰页数。私有文件映射中未复制的文件页属于文件对象，不计入 Mapped。
- getrusage 的缺页计数来自成功 restricted-mode trap：minor=全部成功故障减 pager 等待，major=pager 等待。normal-mode user-copy 不在该计数中；不以块设备读取量伪造缺页次数。
