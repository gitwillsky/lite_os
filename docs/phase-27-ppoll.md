# Phase 27：multi-source ppoll readiness

## 目标与所有权

实现 musl/BusyBox 使用的 Linux `ppoll(73)`，但不建立 poll 专用 scheduler queue。一次 ppoll 只创建一个 `WaitMembership::Poll` 和一个 IndexedWaitEntry；entry 可以同时挂入多个 Pipe identity/direction index 与 Console index，并可附带 deadline。任一 source、timeout 或 signal 先移除 entry 后，会同步清理其他全部 index。

## 协议

syscall 先导入至多 1024 个 pollfd，并解析可选 relative monotonic timeout 和临时 signal mask。regular inode 按请求立即可读/可写；Terminal 提供 cooked/raw input readiness；Pipe 根据 endpoint direction 报告 POLLIN/POLLOUT，并无条件附加 HUP/ERR；negative fd 忽略，invalid positive fd 返回 POLLNVAL。

没有 ready fd 时，registry lock 内重新计算全 fd set readiness，然后发布唯一 Poll membership。Pipe state change/endpoint close 与 UART deferred softirq 沿原 index wake；timeout 沿 deadline index；deliverable signal 沿统一 interruption path。因此多 source 不会只等待第一个 fd，也不存在 check/enqueue lost wakeup。

可选 signal mask 复用 Thread 的 suspend-restore owner：ready/timeout 立即恢复旧 mask；signal interruption 保留 restore record，由实际 Linux signal frame 保存旧 mask 并在 rt_sigreturn 恢复。显式忽略及默认忽略 signal 不再错误中断 blocking wait。

## 证据

固定 musl consumer 创建两个 Pipe，zero-timeout ppoll 先证明无事件，再由 child 延迟写第二个 Pipe，blocking ppoll 必须只报告第二个 POLLIN。BusyBox gate 明确禁止 syscall 73 缺失，并继续通过管道、重定向、后台 wait 与 Ctrl-C。
