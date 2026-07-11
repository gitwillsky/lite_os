# Phase 24：SIGCHLD 与统一 signal wait

## 目标

让未修改的 BusyBox init 可以按 musl 的标准 `rt_sigtimedwait` 路径阻塞等待 delayed signal，删除 syscall 137 缺失造成的轮询空转。实现必须复用 Thread pending state、process exit commit 与 indexed wait registry，不建立 BusyBox 专用通知或第二套 signal queue。

## 单一所有权

Thread 的一个 `PendingSignals` mutex 同时拥有 standard-signal bit 与每个 signal 的首个 `PendingSignal`。重复 standard signal 仍按 Linux 规则 coalesce，且不会出现 bit 已发布、siginfo 尚未发布的中间状态。显式 `SIG_IGN` 在入队前丢弃；默认 SIGCHLD 必须保留到 blocked `rt_sigtimedwait` 消费，若未屏蔽则仍由 trap-return 默认 disposition 消费并忽略。

最后一个 child Thread 退出时，process graph 在一次提交中写入 exit record、取走 wait4 waiter并定位 live parent signal target。释放 graph lock 后发布 `SIGCHLD/CLD_EXITED`，因此 signal wake 不会反向获取 process graph lock。wait4 与 SIGCHLD 是同一退出事实的两个标准观察接口，不是两套 child lifecycle。

## 等待协议

`rt_sigtimedwait` 的 `Signal` membership 进入 TaskManager 唯一 indexed wait registry；有限 timeout 复用现有 absolute deadline index。协议固定为：

1. registry owner lock 内先消费匹配 pending signal；
2. 再检查 timeout 与无关的可交付 signal；
3. 最后发布 Signal membership 并阻塞。

sender 先发布 pending state，再获取 registry owner lock复查 matching set 并移除 registration。timeout、matching signal 与 unrelated signal interruption 只有先移除 membership 的路径能发布 wake result，因此没有 signal-before-enqueue lost wakeup 或双重消费。

## ABI 与边界

- syscall 137 接受 8-byte sigset、可选 128-byte RV64 `siginfo_t` 与可选 relative monotonic timespec；
- zero/expired timeout 返回 `EAGAIN`，无关可交付 signal 返回 `EINTR`；
- `tgkill` 提供 `SI_TKILL + sender TGID`；正常 child exit 提供 `SIGCHLD + CLD_EXITED + child TGID + status`；
- SIGKILL/SIGSTOP 从 wait set 去除。

当前仍是 standard-signal coalescing，不支持 realtime queued value、process-directed target selection，也没有定义完整多线程 parent 应由哪个 Thread 接收 SIGCHLD，因此 syscall 矩阵保持 Partial。

## 验收

固定 musl v1.2.6 consumer 真实执行 zero-timeout `EAGAIN`、blocked `SIGUSR2/SI_TKILL`、child exit `SIGCHLD/CLD_EXITED` 和 waitpid 回收。固定 BusyBox 1.37.0 init+ash gate 明确禁止出现 `unsupported syscall_id: 137`，随后仍需通过 UART 注入命令并得到 ash 运算结果。`make verify` 继续覆盖 architecture fence、Clippy、三组件构建、1/3/8 hart 启动、musl consumer 与 BusyBox gate。
