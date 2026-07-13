# Phase 59：多线程 musl process spawn lifecycle

## 目标

让动态 musl 在 parent sibling Thread 持续运行时，按标准路径完成 `system`、`popen`、`posix_spawn/posix_spawnp`、file actions、exec failure errno handoff 与并发 `waitpid`，不保留近似 vfork 或单线程 wait 双轨。

## 实现

- `CLONE_VM|CLONE_VFORK|SIGCHLD` child 是独立 Process，但精确共享 parent 的同一 AddressSpace Arc；共享 mm 内的 supervisor trap page 按全局 TID 唯一分配。
- exec 在 point-of-no-return 前完整构造并注册新 AddressSpace，随后只替换 child Process handle、删除旧 mm 临时 trap page，再恢复发起 vfork 的 parent Thread；exit 路径同样先清理临时页。
- process graph 为每个 parent TID 保存 child waiter；child event 在锁内授予唯一 claim，status copyout 成功后消费，失败则 release 并唤醒其他 waiter 重新检查。
- syscall 283 实现并只宣告 `QUERY`、`REGISTER_PRIVATE_EXPEDITED`、`PRIVATE_EXPEDITED`。registration 归属 AddressSpace；arch 以单调 generation + SBI IPI 同步所有 active DTB hart 的 full fence，并允许并发 syscall caller 在关中断等待期间主动完成 pending request。
- 删除旧 `try_clone_for_vfork` 独立页表/共享 frame 路径，以及 vfork/wait4 的多线程 `EAGAIN` 限制；普通多线程 fork/exec 仍按 ABI 矩阵明确返回 `EAGAIN`。

## Guest gate

固定动态 consumer 在一个后台 pthread 持续 runnable 时执行：

1. fresh exec mm 的 membarrier `QUERY`、注册前 `EPERM`、pthread 注册后的 private expedited barrier；
2. `system`、`popen`、`posix_spawnp` PATH search 与 stdout `addopen` file action；
3. 不存在映像通过 musl CLOEXEC error pipe 返回 `ENOENT`；
4. 三个 parent waiter 并发等待两个 gated shell child，其中两个竞争同一 child，必须恰好一个成功、另一个 `ECHILD`，另一个 child 独立回收；
5. 最终输出 `LITEOS_POSIX_SPAWN_59`，host 只等待 marker 并拒绝 kernel error/unsupported syscall。
