# Phase 14：process graph、fork-shaped clone 与 wait4

## 目标与边界

本阶段接入 Linux/riscv64 `clone(220)` 与 `wait4(260)` 的 process 竖切。只支持 `clone(SIGCHLD,0,0,0,0)`；thread、vfork、TLS、parent/child TID pointer、futex 与 signal status 留给各自完整竖切，不保留同名占位状态。

## 唯一 owner

- TaskManager 的有锁 process graph 唯一拥有单调 PID allocator、parent edge，以及每个 PID 的 `Live(TaskControlBlock)` 或 `Exited(status)` 状态。
- 不维护反向 child collection。child 查询在 graph 中按 parent edge 选择，orphan reparent 也只改写该 edge。
- 完整 TCB 不作为 zombie 保留。exit 后 graph 只保存 PID、parent 和 normal exit code，kernel stack/address space/fd table 通过 deferred reap 释放。
- `SchedulingState::wait` 是 deadline 和 child wait 共用的唯一 `WaitMembership`；process graph 只在 parent 阻塞期间拥有 waiter Arc。

## 创建与等待事务

1. fork 在 parent 不变的前提下 eager 深拷贝有序 VMA、物理页、cwd 与 fd entries；fd entry 共享原 OFD 的 offset/status flags。
2. child 使用独立 kernel stack、页表和 TrapContext，从已前移 syscall PC 返回，`a0=0`；parent 在 child 完整准备后才发布 graph node 与 runqueue membership。
3. wait 在 process graph lock 内完成“检查 exit record → 发布 waiter → Blocking”事务；exit 取得同一锁后提交 record并取走 waiter，因此不存在检查与睡眠之间的 lost wakeup。
4. wait status 先 copyout，成功后才移除 exit record；`EFAULT` 不丢失可再次等待的 child 状态。

## 启动验收

`/bin/init` fork child；child 验证 `getppid()==1` 后以 23 退出。parent 阻塞等待并验证 PID、`23<<8` status、eager stack copy 的内存独立性，以及 record 只能消费一次。QEMU `-smp 1/3/8` 都必须输出 `process ok`。

## 验证结果

`make verify` 已通过：格式、RISC-V workspace check、Clippy `-D warnings`、架构/接口围栏、三组件构建、ELF 静态检查、`git diff --check`，以及 QEMU `-smp 1/3/8` 冷启动均成功。
