# io_uring

## 当前设计

io_uring 是 Linux ABI 里对应 Windows NT 完成端口的接口：用户态把请求写进提交队列（SQ），内核把结果投到完成队列（CQ），
双方只通过共享内存里的 head/tail 交接，不必为每个请求进内核。已实现骨架、`NOP`、读写类 opcode、`FSYNC`、`CLOSE` 以及阻塞请求的异步完成，
其余 opcode 的接入按 [io_uring 契约](../architecture-contract/io-uring.md)分期进行。

1. `io_uring_setup` 分配两块清零的 `DeviceBacking`：ring 区域（头部、CQE 数组、SQ array）与 SQE 数组，返回恒带
   `O_CLOEXEC` 的匿名 ring fd。三个魔数 mmap 偏移（`IORING_OFF_SQ_RING/CQ_RING/SQES`）只在 ring fd 的 `DeviceFile::mmap`
   里解释；映射持有 backing，因此关闭 fd 后已映射的页仍然有效。
2. 内存序遵循 Linux 约定：SQ 的 tail 由用户态 release 写、内核 acquire 读，head 反向；CQ 同理。内核先写 CQE 再 release
   写 tail，用户态 acquire 读 tail 后才读 CQE；head/tail/flags 全部经 `AtomicU32` 访问。
3. `io_uring_enter` 先提交再等待。提交在 `submit_lock` 下批量消费 SQE：下标越界的条目记入 `sq_dropped`，init/prep 失败的 SQE
   产生错误 CQE 并停止本次提交（Linux 在没有 `SUBMIT_ALL` 时同样如此），整批处理完后一次 release 写 head。
4. CQ 环满时 CQE 按序进入内核溢出链表并置 `IORING_SQ_CQ_OVERFLOW`，用户态下次 `GETEVENTS` 时搬回环；链表有 8 倍 CQ 容量
   的上限，超出后丢弃并以一次 `EBADR` 通知。链表非空时新 CQE 必须排在其后，保持完成顺序。
5. 等待用合并 readiness Pipe：先排空再复查条件再睡眠，发布 CQE 的一方重新置位，因此不会丢唤醒。同一个 Pipe 也是 poll/epoll 的
   唤醒源，ring fd 在 CQ 有完成时可读、SQ 有空位时可写。
6. `fs` 不依赖 `io_uring`：`syscall` 从 `Arc<dyn DeviceFile>` 经 `Any` 向下转型取回 ring。
7. 请求在 prep 后解码成 `Now`（不会等待，直接执行）或 `Wait`（一个 future，先在 `io_uring_enter` 内 poll 一次，未完成才交给 worker）；读写复用 `read(2)`/`write(2)` 的同一个核心（多一个 `nowait`，不改共享 OFD 的 `O_NONBLOCK`）。
   完成就发布 CQE；仍会等待（空 pipe、尚未触发的 poll/timeout 等）就写成一个 `async fn` future 交给 `io_uring` 内核线程：读写是“尝试一次，`EAGAIN` 就 `await` 就绪”，
   `POLL_ADD` 是“探测 revents，没有就 `await`”，`TIMEOUT` 是“计数或 deadline 到了就返回，否则 `await`”。一个多路复用线程而不是线程池：N 个阻塞读都在等同一个后续写时，固定大小的池会死锁。
8. readiness 来源没有回调（是 task 睡眠/唤醒式的），所以该线程是 reactor 而不是 waker 驱动的执行器：future 在 `await` 时把等待条件登记到自己的 interest 槽
   （目标 OFD 的事件、deadline、CQE 计数），线程据此构造 `wait_for_poll` 的 key，条件满足后才重新 `poll`，`Context` 使用 `Waker::noop()`。
9. 持久文件（`OpenFileKind::Inode`）的读写可能睡眠在磁盘上，不在提交线程或 reactor 上执行：读（显式偏移或 `off == -1` 的当前位置）走 `Wait::FileRead`——cache 命中同步复制，
   未命中开始异步填页（块层 `begin_read` + page-cache 版本协议）并以 `Interest::Block` 让出，块读完成时通知该链的 `Waiter`；写、fsync 与回退的读走 `Wait::Lane`，
   交给 `io_uring_lane` worker 池：共享一个 FIFO，没有空闲 worker 且未达上限 `MAX_WORKERS` 时新建内核线程，所以一个慢作业只占一个 worker，写与 fsync 可以重叠
   （Linux 的 ext4 缓冲写与 fsync 同样由 io-wq 执行）。reactor 因此从不睡眠。
   当前位置读不在读期间持有 OFD position（同步 `read(2)` 才持有，reactor 不能睡眠在它上面）：发起时快照 position，完成时发布 `起点 + 读到的字节`，与 Linux io_uring 相同；
   position 此刻被同步读写占用时，发起与发布都改走车道。
10. 一条请求链（单个 SQE 是长度为 1 的链）同一时刻至多一个成员在等待，所以整条链是一个 future：成员按序 issue，失败的成员取消后继。`IOSQE_IO_DRAIN` 通过 ring 上的接纳序号表达，
   被延迟的链作为 future 的第一个 `await` 等待被接纳。取消经登记表给正在等待的成员置位取消标记，不经命令队列，因此取消也是普通的链成员，可以出现在链里。
   丢弃 future（ring 关闭）会释放它捕获的目标 OFD 和 iovec。ring fd 的最后一个引用释放时 `DeviceFile::release` 置关闭标志并唤醒该线程，它丢弃这个 ring 的全部请求（例如 pipe 读端被释放，使写者得到 `EPIPE`）。

## Known limits

- 未开放：multishot 的 poll/timeout、按 fd/opcode 取消、其余 opcode、`LINK_TIMEOUT`、`BUFFER_SELECT`、固定文件/缓冲区注册、SQPOLL/IOPOLL、SQE128/CQE32。
- 阻塞车道最多 `MAX_WORKERS`（8）个线程：超过后作业在共享 FIFO 里排队（有界并发），空闲 worker 不退出。regular 文件的写与 fsync 在 worker 上同步执行，不是真异步（Linux io-wq 同样）。
- regular 文件读只在 page cache 未命中且后端支持异步读（ext4；其固定 4 KiB profile 保证文件系统块恒等于设备块）时不占线程，其余后端与请求槽暂满时回退车道。
- 同时发起的两个当前位置读读取同一区间（Linux 相同）：需要顺序语义的用户应使用链。
- signalfd 读无信号时返回 `-EAGAIN`，不挂起。
- SQ 上限 4096（Linux 32768）：没有按用户的 ring 内存计费，缺少上限时一个进程创建多个 ring 即可耗尽物理内存。
- `io_uring_enter` 的 `min_wait_usec` 与 `REGISTERED_RING` 未实现，返回 `EINVAL`。
