# Phase 26：Pipe、向量 I/O 与 shell jobs tracer bullet

## 单一模型

`ipc::Pipe` 唯一拥有 64 KiB ring、read/write endpoint 计数与 4096-byte `PIPE_BUF` 原子写规则。`PipeEnd` 只由普通 OFD 持有，dup/fork 共享 OFD Arc；最后一个 endpoint Drop 发布 EOF 或 broken-reader readiness。Pipe 不拥有 Task，TaskManager 不拥有数据，只通过 notifier 把 Pipe identity/direction 接入唯一 indexed wait registry。

blocking read/write 在 registry lock 内复查 Pipe readiness再发布 `WaitMembership::Pipe`。数据读写和 endpoint close 都先释放 Pipe state lock，再通知 registry，因此 lock order 固定为 registry → Pipe，且 close/enqueue、signal/enqueue 不丢 wake。O_NONBLOCK 返回 EAGAIN；无 reader 写入发布 SIGPIPE 并返回 EPIPE。

## 进程退出修复

Pipe gate 暴露出原 Process fd table 依赖 TCB 最终 Drop：scheduler 为安全 kernel-stack reap 保留的 Arc 会延迟 writer close，导致已退出 child 的 pipe peer 永远等不到 EOF。最后一个 Thread 现在在 process exit commit 后立即从 files lock 取走整个 fd table，并在锁外 Drop；TCB 内存仍可延后回收，但 POSIX fd lifecycle 已完成。

## ABI 与证据

新增 `pipe2(59)`、`readv(65)`、`rt_sigsuspend(133)` 和当时的 identity getter 174-177；getter 已在 Phase 44 改为 Process credentials。`readv/writev` 共享 OFD 后端，Pipe readv 一次读取后 scatter，已有进度不会为后续 iovec 再阻塞。`rt_sigsuspend` 与 `rt_sigtimedwait` 共用 Signal membership，但不消费 pending bit；signal frame 恢复 suspend 前 mask。

固定 musl consumer 证明 `writev → readv` 两个 iovec、child exit 自动关闭 writer、EOF 与 waitpid。固定 BusyBox gate 证明 `/bin/echo | /bin/grep`、`>` 持久化重定向、后台 subshell、`wait`、后续 `cat`，并继续证明 foreground Ctrl-C。输入不包含最终算术 marker，避免终端 echo 伪造成功。
