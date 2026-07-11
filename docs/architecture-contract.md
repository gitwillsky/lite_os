# LiteOS Architecture Contract

本文只定义稳定的 module interface、依赖和状态 owner。当前功能事实属于 `architecture.md`，Linux ABI 状态属于 `syscall-support.md`。

## 1. Crate contract

- `bootloader` 是独立 M-mode domain，不依赖 kernel 或 user。
- `syscall-abi` 只保存用户可见 Linux/riscv64 ABI 常量，不依赖实现 crate。
- `user` 只依赖 `syscall-abi` 与自身 runtime。
- kernel 的 `main.rs` 是唯一 composition root；初始化顺序和 adapter 装配不得下沉到 driver、filesystem 或 task。

## 2. Kernel dependency contract

| Module | 允许依赖（机器读取） | 说明 |
|---|---|---|
| `arch` | `config`, `memory`, `sync` | architecture mechanism 不得消费上层领域状态 |
| `config` | 无 | 只保存无运行时依赖的常量 |
| `sync` | `arch` | 只依赖本地中断机制 |
| `memory` | `arch`, `config`, `id`, `sync` | 不感知 task、filesystem 或 driver policy |
| `drivers` | `arch`, `memory`, `sync` | 不感知 task、filesystem 或 syscall |
| `ipc` | `sync` | 只拥有 Pipe data/lifecycle，不感知 fd、task 或 syscall |
| `fs` | `drivers`, `ipc`, `sync`, `timer` | `drivers` 仅允许 `block` seam |
| `task` | `arch`, `fs`, `ipc`, `memory`, `sync`, `timer` | 不依赖具体 device 或 syscall/trap entry |
| `trap` | `arch`, `drivers`, `memory`, `syscall`, `task`, `timer` | 只做入口、分类和事件投递 |
| `syscall` | `fs`, `ipc`, `memory`, `task`, `timer` | 不得绕过 facade 接触 adapter/scheduler/page table |
| `timer` | `arch`, `config`, `drivers`, `sync` | RTC adapter 由 timer 唯一拥有 |
| `log` | `arch`, `sync` | 日志策略和输出在本 module 内闭合 |
| `id` | 无 | 纯 ID allocation mechanism |
| `lang_item` | `arch` | 只使用 architecture fail-stop mechanism |
| `main` | `arch`, `config`, `drivers`, `fs`, `id`, `ipc`, `lang_item`, `log`, `memory`, `sync`, `syscall`, `task`, `timer`, `trap` | 唯一 composition root |

同一 module 内引用不构成跨 seam 依赖。`main.rs` 可以依赖所有 kernel module，但只能做装配、启动顺序和 fail-stop 策略。

## 3. State owners

| 状态 | 唯一 owner |
|---|---|
| M-mode initialization、HSM、RFENCE | bootloader 对应 module |
| DTB board facts | immutable BoardInfo publication |
| hart possible/online/active、startup stack | HartTopology |
| per-hart current、runqueue、mailbox | task ProcessorTopology |
| task run state、generation、wait membership 与 wake result | SchedulingState |
| process address space、cwd inode、fd table | Process；最后一个 Thread exit 立即取走 fd table，TCB 延迟析构不得延迟 fd close |
| PID/TID allocation、parent edge、live thread collection 或最小 exit record、child waiter | TaskManager process graph |
| deadline/futex/pipe/signal/console wait registration 及其 indexes | TaskManager 唯一 IndexedWaitQueue；SchedulingState 保存唯一 ID |
| signal disposition | Arc<Process> signal-actions table |
| signal mask、pending set、active frame | ThreadContext 与用户 RV64 rt_sigframe |
| interrupted syscall 的单次 replay record | ThreadContext；signal frame 保存最终 replay/EINTR 上下文 |
| OFD offset/status flags | OpenFileDescription |
| anonymous Pipe byte ring、endpoint count、PIPE_BUF atomicity | ipc::Pipe；不复制到 fd table 或 wait registry |
| VMA 区间、类型、权限与 framed page lifetime | MemorySet 的有序 VMA 表；PageTable 只保存硬件 translation |
| physical frame lifetime | FrameTracker/frame allocator |
| root mount、pathname traversal | VFS |
| inode/on-disk allocation state | filesystem adapter mutation domain |
| VirtIO descriptor/DMA lifetime | VirtQueue/driver instance |
| UART MMIO 与固定容量 RX ring | UART driver；hardirq 只填 ring，console waiter 只由 deferred softirq 消费 |
| interrupt registration/affinity | interrupt controller |
| syscall number | syscall-abi |
| user pointer与 errno translation | syscall module |

新增 global、Atomic、lock、cache 或 flag 必须在声明附近写 `OWNER:`，并说明缺失该 owner 会造成的具体状态分裂。

## 4. Interface and capability contract

- kernel 与 bootloader 是 binary crate，跨 module interface 使用最窄的 `pub(super)`、`pub(in …)` 或 `pub(crate)`；不得使用裸 `pub` 伪造外部 interface。
- 默认 private；Rust AST 围栏解析所有 scoped visibility declaration、字段、方法、trait item 与 enum variant，连同可见域由 `architecture-interface.txt` 完整记录。
- filesystem 只能看到 `drivers::block` seam，不得看到 VirtIO adapter。
- MMIO/volatile 只存在于 arch/driver HAL；user pointer 只通过 AddressSpace copy；磁盘 packed layout 只存在于 filesystem adapter。
- syscall memory handler 只解析 Linux flags/prot/errno；TaskControlBlock/AddressSpace 只持锁转发；VMA 选址、冲突、split/merge、frame rollback 与 PTE 提交只存在于 MemorySet。
- thread-directed signal 先发布 pending bit，再从 wait 的唯一 owner 注销 membership；blocking path 必须在 owner lock 内复查 deliverable signal，禁止 signal-before-enqueue lost wakeup。
- UART hardirq 不调度、不分配，只清空设备 FIFO并发布 console softirq；console read 在统一 indexed wait owner 内复查 RX ring，deferred consumer 才移除 membership 并 wake task。
- syscall handler 只能向 dispatcher 返回内部 restart 结果；trap layer 将其暂存为 `EINTR` 并把原 `a0..a5/a7/ecall PC` 交给当前 Thread。实际交付的 handler disposition 含 `SA_RESTART` 时才把 replay context 写入 signal frame，否则 frame 保留 `EINTR`；内部结果不得进入 U-mode。
- Thread exit 发布顺序固定为 robust cleanup -> process graph removal -> clear-child-tid/futex wake；join completion 不得早于 Thread owner 注销。
- raw CSR、DMA、page-table pointer、trap context 和 packed disk unsafe 必须有局部 `SAFETY:` 证明。
- 禁止 `static mut`、私有 syscall、固定 hart 容量、console syscall 旁路、deprecated/feature-flag 双轨。
- 禁止 `common/utils/helpers/misc/manager/base/shared/core` 等无领域含义的目录。

## 5. Change contract

修改前必须确定所属 module、状态 owner、interface 变化、依赖变化以及 error/exit/interrupt cleanup。依赖采用正向 allowlist，未列出的跨 module 依赖一律失败。扩大 interface 或新增 global state 必须更新对应权威契约；不得修改围栏来掩盖实现问题。只有其他架构规则全部通过时，`cargo run -p architecture-check -- --write-interface` 才能更新 interface contract。

唯一强制验证入口是 `make verify`。
