# io_uring 当前架构

io_uring 的 Linux UAPI、共享 ring 与请求执行全部位于 `system/linuxd`；内核只提供 VMO、
承载线程、futex 和等待机制。能力与限制以[文件 I/O ABI 矩阵](../syscall-support/filesystem-io.md)为准，
具体所有权见[io_uring 契约](../architecture-contract/io-uring.md)。

## 提交与完成

1. `io_uring::ring` 拥有 SQ 消费、CQ 发布、溢出链表与接纳序号；`DeviceBacking` 以 VMO 保存
   ring/SQE 页。mmap 持有实际 backing 与 OFD，关闭 fd 不撤销已有映射。
2. SQ/CQ 的 head、tail、flags 经原子访问；producer 先写记录再 release 发布 tail，consumer
   acquire 读取 tail 后才消费。SQE 在发布 head 前复制到 linuxd，后续执行不再读取用户改动的槽。
3. `syscall::io_uring` 解码请求、取得提交者能力并执行链。已完成的操作直接发布 CQE；会等待的链
   是一个 future，先在提交者上 poll，之后由唯一 reactor 推进。取消者只修改等待状态，不 poll future。
4. reactor 的 slots 只由 reactor 退休。首次 poll 同步完成时提交者清除 future 并通知 reactor，
   不改变它在锁外遍历时保存的下标。future 的锁串行化首次 poll 与后续推进，完成时撤下旧 interest。
5. 合并 Pipe 只表示状态可能变化；等待前排空通知并复查实际状态。一次 source 通知使用同一 ticket
   边界，新登记不能被旧的 readiness 快照反复唤醒。具体协议属于 WaitRegistry。

## 文件 I/O 与阻塞边界

- pipe、socket 等流式 I/O 先作 per-operation NOWAIT 尝试；需等待时登记 OFD readiness，
  不修改共享 OFD 的 O_NONBLOCK。一个 reactor 同时驱动全部等待链，避免有限线程池被互相依赖的读占满。
- regular buffered read 先经统一文件读取 owner 尝试有效、已驻留的缓存。VMO 的非等待读取与撤销
  受同一树锁保护；完整成功后才交付暂存数据，缺页不发 pager 请求，缓存有效性仍属于 inode。
- 缺页、重新验证、direct I/O、position 争用与超出 inline 暂存预算的请求，以及 write/fsync，
  交给有界阻塞车道。车道的普通读取触发同一个 pager VMO，由 linuxd pager 经文件系统服务补页。
- sequential read 在一次完整 I/O 的 OFD position 临界区中复制并推进；显式偏移不改变 position。
  缓存探测失败不发布用户数据或 position，不另建缓存页索引。
- 作业结果槽先发布结果再通知链；future 先清通知再取结果，防止清掉已经完成作业的唯一唤醒。
  排队取消与开始执行争夺同一次原子状态转换，已开始的作业返回真实结果。

## 生命周期与验证

ring final fput 关闭并唤醒 reactor，它丢弃挂起链及其 OFD/向量引用；已开始的车道作业持有自己的
资源到完成。通知引用环由作用域守卫拆除，旁观 Arc 不延长设备或文件 admission。
DRAIN、LINK/HARDLINK、取消和超时在同一链状态中实现，不用平行命令队列同步另一份状态。

musl runtime 执行真实共享 ring，覆盖冷读/缓存命中、溢出、链接、取消、设备命令、进程退出，以及
多个提交者的同步首次完成与 reactor 异步退休交错；宿主 fixture 负责实际 owner 的事务和寿命边界。
