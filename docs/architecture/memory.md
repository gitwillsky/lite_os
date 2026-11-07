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

折叠复用已有分块页列表，转交仍可见且未被覆盖的页；偏移相加，可见窗口求交。窗口在缩短时收紧，之后增长
不能重新暴露被截断祖先的内容。页框地址保持不变；被覆盖页只在映射撤销和 fence 完成后才可释放。

## 地址空间与退休

VMAR 持有区间树、映射权限与创建时的 rights 上限。共享地址空间管理高地址区，各成员独立管理低地址区；
成员根入口按需借用共享 owner 的下级子树，normal/private 视图共用私有区下级表。
共享撤销只修改一次叶项，以全 CPU/ASID fence 退休；不逐成员复制叶项或扇出撤销。

锁序为 VMO 树 → 地址空间；共享地址空间先于其成员。缺页先找到并保活 Mapping，释放空间锁，再按此顺序解析
VMO 与安装页表。Mapping publication/revalidation 和 translation retirement 的详细约束见[内存契约](../architecture-contract/memory.md)。

## Linux 内存与 pager

每个 Linux `MemorySet` 持有成员进程、根 VMAR 和唯一的 VMA 索引。私有匿名、heap、stack 使用匿名 VMO；
fork 按对象取快照并在同一子空间复用快照，保持拆分 VMA 的偏移与 alias 关系。共享匿名、memfd/tmpfs/SysV
共享内存使用同一对象；文件私有映射使用页缓存 VMO 的 at-least-on-write 子对象。

文件内容由其 owner 的 pager VMO 支撑（Fuchsia fxfs 与 Starnix RemoteFs）：
- ext4 服务挂载的普通文件由 ext4 实例拥有内容：每文件一个 pager VMO，ext4 补页、直写存储并同步 VMO、截断/分配时
  调整大小，映射写入的脏页在 fsync、syncfs、5 秒周期、节点释放与拆除时写回。linuxd 的 `DataBacking::SharedVmo` 不建页缓存：
  read 读该 VMO，write 经 FUSE WRITE，mmap 映射挂载控制 `BACKING_MEMORY` 交出的同一 VMO（私有映射取其子对象）。
- tmpfs/memfd 与 Linux FUSE 守护进程的文件仍由 linuxd 的 pager VMO 缓存，脏页经 native dirty query/writeback 回写。
补页线程池（`pager-pool`）在每个内容 owner 进程内共用：分派线程交给可增长的工作线程，避免 pager 与 FUSE 守护进程
自身缺页互锁。设备 DMA 页由驱动宿主使用 BTI/PMT 固定。
每个工作线程持有常驻映射的补页暂存区（`SupplyBuffer`）：存储读直接写入它再整体移交目标 VMO，不经堆缓冲。
预读由内容 owner 决定，内核每次仍只请求缺失的一页并容忍请求范围外的补页：ext4 `Content` 为每文件维护顺序访问窗口
（起步 32 KiB，倍增到 512 KiB，不越过 EOF 页）；linuxd 持久文件按 filesystem 声明的窗口预读。
内核读/执行缺页在解析主页的同一树锁事务内，把同一 64 KiB 对齐窗口中已驻留的文件页只读装入
（不分配、不请求、不覆盖已有翻译；匿名页与私有副本不装），一个 `TranslationCommit` 一次 fence；写入仍走正常缺页置脏。

内核先解决按需提交、COW 和 pager 缺页；仍失败才退出 restricted mode。linuxd 根据 VMA 分类
SIGSEGV/SIGBUS 或扩展 GROWS_DOWN。成功解决的 restricted 缺页及其中的 pager 等待从承载线程 native runtime
信息读取，线程退出时并入进程累计；不再通过服务未收到的异常推测缺页次数。

## 当前限制

不做匿名页换出（与 Fuchsia 一致）。空闲页低于低水位（容量 1/64，至少 256 页）后，后台内核线程回收 pager
支撑 VMO 的 Clean 且未固定页，用户缺页入口保留有界兜底；回到 1.5 倍低水位即停止。
页块分代队列先取最老活动代，采集所有实际别名的访问位，访问过的页晋升到最新代；旧代确认排空后才复用代号。
每批最多 256 个页样本及 64 个 VMO 登记，批次间 yield；无进展时等待 10 ms，正常时睡眠而无周期唤醒。
实际水位边沿经 boot CPU deferred 更新原生压力事件，component-manager 记录等级变化；
淘汰先撤销翻译并完成 fence，之后访问由 pager 从存储补回。完整所有权及扫描边界见[内存契约](../architecture-contract/memory.md)。
`swapon`/`swapoff` 明确返回 ENOSYS，
`/proc/swaps` 不存在，Swap 字段为零。用户可观察的内存统计、NOWAIT、RSS 与 fault accounting 边界以
[内存 ABI](../syscall-support/memory.md)及[进程 ABI](../syscall-support/process-identity.md)为准。
