# 读码地图：从哪里开始理解这个内核

本文件面向想通过阅读来学习操作系统的人。它不重复事实，只回答一件事：按什么顺序读，每一章抓住什么思想。
事实的权威来源仍是 [文档索引](README.md) 里的架构与契约文档。源码路径相对 `kernel/src/`。

## 怎么用

- 每一章只有一个核心想法。先读指向的 `//!` 模块头部，它们用同一结构写成：解决什么问题、核心想法、机制、阅读顺序。
- 遇到不认识的词先查 [术语表](glossary.md) 的“架构与内核”一节。owner、seam、publication、fence 这几个词贯穿全书。
- 每章末尾有一道练习，答案紧跟其后。先自己想，再对照代码核实，不要只信答案。
- 第一遍不要读 `namespace/`、`socket/`、`fs/ext4/`、`drm/`，也不要读 `syscall/` 的细节。它们是为兼容 Linux 而来的必要复杂度，不是核心思想。

## 全景

```text
用户态程序
   │ syscall / 缺页 / 中断
   ▼
trap ───────▶ syscall      memory        task
(入口与分派)   (ABI 与错误)   (物理页 / 页表 / VMA)   (调度 / 睡眠唤醒 / 进程)
   ▲                           │                  │
   └────── hardware::arch ─────┴── platform ──────┘   ← 编译期选定的后端，通用代码不知道具体硬件
```

依赖只能自上而下。通用内核只通过 `arch` 与 `platform` 门面使用硬件，没有运行时分派。

## 第 1 章：启动，以及“硬件接缝”

核心想法：用类型而不是约定来保证启动顺序，用编译期选择而不是运行时分派来隔离硬件。

- 读 `main.rs` 的 `kernel_main`：每一步只接受前一步返回的令牌，顺序错了无法编译。
- 读 `arch/mod.rs`：同一份通用代码如何在 AArch64 与 RISC-V 之间切换。
- 读 `hal/console.rs`：一个很小的接缝（`SerialOutput`）长什么样。

练习：为什么“启动顺序用令牌表达”比“在注释里写顺序”更可靠？提示：想想一个新人把两行调用换了位置会发生什么。

## 第 2 章：一次 trap 的完整旅程

核心想法：中断处理函数只确认硬件并登记工作；真正的工作在返回用户态之前的安全点统一做。

- 读 `trap/mod.rs` 头部，然后读 `handle_user_trap` 与 `trap_return`。

练习：用户程序读一个没有映射的地址，最终收到什么信号，`si_code` 是什么？
答案：`SIGSEGV`，`SEGV_MAPERR`（见 `handle_user_page_fault`）。物理页耗尽则是 `SIGKILL`，而不是 `SIGSEGV`。

## 第 3 章：物理内存与伙伴系统

核心想法：块大小是 2 的幂且按自身大小对齐，于是伙伴只需一次异或就能找到。

- 读 `memory/frame_allocator.rs` 头部，再读 `alloc_order` 与 `dealloc_contiguous`。

练习：管理区间正好是页号 16..32 的一个 order 4 块。分配 order 1 之后，各 order 的空闲链表里有什么？再把它释放，会合并几次？
答案：切分得到 order 3 的 `{24}`、order 2 的 `{20}`、order 1 的 `{18}`，分配到 `16`。释放 `16` 时依次与伙伴 `18`、`20`、`24` 合并 3 次，回到 order 4 的 `{16}`；再往上的伙伴 `0` 在管理区间之外，合并停止。

## 第 4 章：地址翻译与 TLB 一致性

核心想法：撤销映射要先让所有 CPU 忘掉旧翻译，才能释放物理页。

- 读 `memory/page_table.rs`（薄适配，真正的页表在 `arch` 里），再读 `memory/mm/shootdown.rs` 头部。

练习：为什么权限放宽（`Relax`）只需要本地 fence，而权限收紧（`Revoke`）要通知所有 CPU？
答案：放宽时别的 CPU 缓存的旧条目最多多触发一次保护异常，无害；收紧或换页时旧条目还能访问不该访问的内存，必须确认它已失效。

## 第 5 章：fork 与写时复制

核心想法：fork 时只共享并去掉写权限，第一次写才复制；“是否独占”由真实的映射记录决定。

- 读 `memory/mm/mmap.rs` 头部：VMA 如何建立、拆分与回收，以及为什么“先预留、再修改”。
- 读 `memory/mm/cow.rs` 头部，再读 `memory/mm/resident.rs` 与 `memory/mm/private_fault.rs`。

练习：fork 之后父进程先写一页，再由子进程写同一页，分别走写缺页的哪一种情况？
答案：父进程写时页被共享，走“复制”；父进程换到新页后，子进程成为旧页唯一的映射者，走“独占，直接加回写权限”。

## 第 6 章：可失败的数据结构

核心想法：把事务里唯一会失败的一步（分配节点）提前，提交阶段就不再失败。

- 读 `fallible_tree/mod.rs` 头部，再读 `topology.rs` 的旋转，最后读 `commit_vacant`。

练习：为什么 `commit_vacant` 遇到已存在的 key 要 panic，而不是返回错误？
答案：提交阶段的契约是“不会失败”。key 重复说明调用者的事务逻辑已经出错，继续运行只会让状态更坏，所以 fail-stop。

## 第 7 章：调度

核心想法：总是运行 vruntime 最小的任务；队列里不删除离开的任务，而是让它们变成过期项。

- 读 `task/scheduler/cfs_scheduler.rs` 头部，然后读 `task/model/scheduling.rs` 里的 `transition_to_ready`。
- 再读 `task/scheduler/placement_policy.rs`：为什么 N+1 个任务需要周期性轮转，而 N 个任务不需要。
- 想知道“没有空闲 CPU 主动拉取”的缺口有多大，运行 `make diagnose-scheduler-latency`，它给出可重复的数据。

练习：当前任务的 vruntime 是 250，队首是 200，会抢占吗？队首也是 250 呢？
答案：前者会，后者不会（`local_ready_preempts` 要求严格小于）。

## 第 8 章：睡眠与唤醒

核心想法：先登记，再复查条件，最后才睡；用一次 CAS 在多个唤醒来源里决出唯一赢家。

- 读 `task/process_table/wait_registry.rs` 头部，再读 `arm_current`。

练习：去掉“发布之后的复查”会出什么 bug？
答案：条件检查与登记之间发生的事件既没被检查看到，也找不到尚未登记的等待者，等待者会睡死（丢失唤醒）。

## 第 9 章：并发与上下文

核心想法：执行上下文决定了能做什么——硬中断里不分配、不睡眠，任务上下文里才能睡。

- 读 `sync/mod.rs`：`IrqMutex` 与 `TaskMutex` 各自适用于什么。
- 读 `sync/task_mutex.rs` 头部：可睡眠的锁为什么要用 `Handoff` 交接而不是“放开再抢”。
- 读 `hardware/deferred.rs`：中断如何把工作留给安全点。
- 再回头读 `memory/mod.rs` 里 `KERNEL_SPACE` 的注释，看一个真实的“持锁时不能等 fence”的例子。

## 之后再读

文件系统（`fs/`、`block/`）、网络（`socket/`）、图形（`drm/`）、进程隔离（`namespace/`）都建立在以上九章的思想之上。
读它们时先看对应的 `docs/architecture/*.md`，再看 `docs/architecture-contract/*.md` 里的接口约定。
