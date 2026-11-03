# Kernel owner 路由

本表是代码导航，不复制架构契约。进入某一行后，必须读取对应 contract 的当前内容，再判断
first broken contract。

| 领域 | 权威 contract | 代码入口 |
|---|---|---|
| boot、trap、CPU、deferred | `docs/architecture-contract/execution.md` | `kernel/src/execution/entry.rs`、`kernel/src/execution/trap.rs`、`kernel/src/execution/cpu/` |
| 原生执行体、scheduler、wait | `docs/architecture-contract/execution.md` | `kernel/src/thread/`、`kernel/src/syscall/task.rs`、`kernel/src/object/` |
| Linux 进程、ptrace、signal | `docs/architecture-contract/process-scheduling.md`、`docs/architecture-contract/ptrace.md` | `system/linuxd/src/task/`、`system/linuxd/src/syscall/` |
| VMAR、VMO、COW、TLB | `docs/architecture-contract/memory.md` | `kernel/src/vm/`、`kernel/src/syscall/memory.rs`、`kernel/src/arch/` |
| Linux 地址空间与用户态 pager | `docs/architecture-contract/memory.md` | `system/linuxd/src/memory/` |
| VFS、page cache、OFD、ext4 | `docs/architecture-contract/filesystem-storage.md` | `system/linuxd/src/fs/`、`system/ext4/src/` |
| epoll、pipe、AF_UNIX、IPv4 | `docs/architecture-contract/ipc-network.md` | `system/linuxd/src/fs/`、`system/linuxd/src/net/`、`system/netstack/src/` |
| 设备 UAPI、TTY、DRM | `docs/architecture-contract/devices-terminal.md` | `system/linuxd/src/devices/` |
| VirtIO、PCI、USB 与 DMA | `docs/architecture-contract/devices-terminal.md` | `system/driver-host/src/drivers/`、`system/device-model/src/`、`system/device-protocol/src/` |
| arch mechanism | 相关领域 contract 与 `docs/architecture-contract.md` | `kernel/src/arch/` |
| machine facts、IPI、设备装配 | `docs/architecture-contract/boot-platform.md` | `kernel/src/platform/` |
| Linux ABI | `docs/syscall-support.md` 路由到领域矩阵 | `abi/linux-abi/`、`system/linuxd/src/syscall/` |

## 调用链追踪

从外向内追踪符号，而不是从怀疑模块向外猜：

```bash
rg -n 'sys_clone|SYSCALL_CLONE' system/linuxd linux-abi
LITEOS_QUERY='clone_process|clone_thread'
rg -n "$LITEOS_QUERY" system/linuxd/src
rg -n 'TaskMutex|try_lock|deferred|notify|wake' kernel/src system/linuxd/src
```

`clone` 与 `LITEOS_QUERY` 是可执行示例；实际运行时替换为红色路径中的 syscall、callee、state
type 或 error variant。

Linux syscall 在 restricted mode 退出后由 linuxd 处理；内核只接收原生对象调用。
跨 channel、pager、BTI 或设备协议继续跟踪真实服务 owner，不把移出的 Linux 领域重新归到内核。

每跨一个 module 记录：

```text
caller：
执行上下文：
传入的语义事件或 ABI：
读取/修改的复合状态：
该状态的唯一 owner：
锁与 wait primitive：
error/exit/interrupt cleanup：
```

## 执行上下文核对

将调用点归入一个上下文，并以对应 contract 证明允许的动作：

- **task**：可以通过明确的 task-only primitive 阻塞，并必须拥有可恢复的 wait membership；
- **trap/hardirq**：只完成有界确认、状态捕获或 work publication；
- **deferred/notifier**：执行有界工作；owner 竞争时使用该领域声明的非阻塞观察、回投或 task
  delivery；
- **boot**：只有 owner 装配完成后才能开放依赖它的中断或调度路径；
- **retirement**：被撤销的 translation、frame、task 或 device owner 保活到所有 reader/CPU
  completion 已确认。

这份列表只用于提出检查问题；允许的具体 primitive 和顺序由领域 contract 裁决。

## first broken contract 记录

在修改代码前填满：

```text
触发条件：
最早发生异常的函数：
CPU / task / tgid：
执行上下文：
期望状态：
实际状态：
被违反的 contract 原文位置：
唯一 owner：
为什么上层错误只是传播结果：
为什么绿色对照不会触发：
```

缺少任一项时，继续缩小边界或增加一个能证伪当前假设的窄探针。
