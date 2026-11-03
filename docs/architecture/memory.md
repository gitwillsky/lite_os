# 内存当前架构

## 机制与策略的 owner

`kernel::memory` 只拥有物理页伙伴分配器、内核堆与地址基本类型；`kernel::vm` 拥有页表、translation
retirement、VMO、VMAR、共享/成员地址空间与 kernel stack。Linux VMA、brk、映射 flags、文件后备、
信号分类、统计与 pager 策略属于 `system/linuxd/src/memory`。内核不保存 Linux mm/VMA 的第二份索引。

## VMO 与 COW

VMO 支持匿名按需提交、pager 支撑、物理连续、物理区间和 slice。快照把已有页移交给不可变隐藏节点，
源与新子对象成为其子节点；双方后续写入独立复制。at-least-on-write 子对象在未写页上观察父对象，
slice 则直接别名父对象的区间。

一棵 COW 树共享一把 task mutex。直接 COW 子对象只以 Weak 登记，强引用沿子到父方向保活内容。
对象取得树锁时折叠仅剩自己的隐藏父节点；创建快照时再检查祖先链，以覆盖“退出状态已通知，但上一承载线程的
映射稍后才退休”的交错。Drop 不等待树锁，也不创建回收队列。

折叠复用已有 AVL 节点，转交仍可见且未被覆盖的页；偏移相加，可见窗口求交。窗口在缩短时收紧，之后增长
不能重新暴露被截断祖先的内容。页框地址保持不变；被覆盖页只在映射撤销和 fence 完成后才可释放。

## 地址空间与退休

VMAR 持有区间树、映射权限与创建时的 rights 上限。共享地址空间管理高地址区，各成员独立管理低地址区；
成员按需把共享映射填进自己的页表。撤销共享映射时，对全部成员撤销对应翻译并完成 fence。

锁序为 VMO 树 → 地址空间；共享地址空间先于其成员。缺页先找到并保活 Mapping，释放空间锁，再按此顺序解析
VMO 与安装页表。Mapping publication/revalidation 和 translation retirement 的详细约束见[内存契约](../architecture-contract/memory.md)。

## Linux 内存与 pager

每个 Linux `MemorySet` 持有成员进程、根 VMAR 和唯一的 VMA 索引。私有匿名、heap、stack 使用匿名 VMO；
fork 按对象取快照并在同一子空间复用快照，保持拆分 VMA 的偏移与 alias 关系。共享匿名、memfd/tmpfs/SysV
共享内存使用同一对象；文件私有映射使用页缓存 VMO 的 at-least-on-write 子对象。

每个持久文件 inode 的页缓存由 pager VMO 支撑。内核请求缺页，linuxd 的分派线程交给工作线程读取后端并补页；
工作线程可增长，避免 pager 与 FUSE 守护进程自身缺页互锁。脏页通过 native dirty query/writeback 协议回写。
ext4 的数据和元数据由 ext4 经 FUSE channel 提供；设备 DMA 页由 driver-host 使用 BTI/PMT 固定。

内核先解决按需提交、COW 和 pager 缺页；仍失败才退出 restricted mode。linuxd 根据 VMA 分类
SIGSEGV/SIGBUS 或扩展 GROWS_DOWN。成功解决的 restricted 缺页及其中的 pager 等待从承载线程 native runtime
信息读取，线程退出时并入进程累计；不再通过服务未收到的异常推测缺页次数。

## 当前限制

没有换出服务，也没有旧内核 swap、后台 reclaim 或 memory-pressure 实现。`swapon`/`swapoff` 明确返回 ENOSYS，
`/proc/swaps` 不存在，Swap 字段为零。用户可观察的内存统计、NOWAIT、RSS 与 fault accounting 边界以
[内存 ABI](../syscall-support/memory.md)及[进程 ABI](../syscall-support/process-identity.md)为准。
