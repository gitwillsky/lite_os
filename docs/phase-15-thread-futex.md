# Phase 15：Thread、TLS、futex 与退出清理

## 目标与范围

本阶段在 process graph 单一路径上增加共享 Process 的 Thread，接入 `set_tid_address(96)`、`futex(98)`、`set_robust_list(99)`，并扩展 `clone(220)` 的 thread-shaped flags。当前 futex 只支持无 timeout WAIT/WAKE；clone 不接受 vfork/namespace/pidfd 等未实现语义。

## 所有权

- `Arc<Process>` 唯一拥有 AddressSpace、cwd 与 fd table；thread TCB 只共享该 owner。
- TaskManager process graph 的 Live 状态持有 `TID -> TCB` collection；最后一个 Thread 退出时才替换为最小 process exit record。
- 每个 Thread 独占 TID、kernel stack、supervisor trap-context 页、TLS `tp`、clear-child-tid 与 robust-list registration。
- Futex queue 以 `(TGID,uaddr,sequence)` 唯一拥有 blocked waiter；SchedulingState 保存相同 `WaitMembership::Futex` token。

## 并发协议

Futex WAIT 持 queue lock 完成用户值读取、waiter insertion 和 Running→Blocking 发布。WAKE 必须取得同一锁后移除 waiter，再通过统一 scheduler wake seam 消费 token。若 wake 早于 WAIT，WAIT 会观察修改后的用户值并返回 `EAGAIN`；若 wake 晚于比较，则必然看到已发布 waiter。

Thread exit 依次处理 robust list、clear-child-tid/futex wake、process graph thread removal、trap-context unmap 与 idle-stack deferred reap。robust owner word 使用用户页上的原子 compare-exchange 设置 `OWNER_DIED`，链表遍历上限为 2048。

## 启动验收

init 创建独立四页用户栈和共享 probe 页；parent/child 通过两个 futex 完成双向阻塞唤醒，child 验证 TLS `tp`、注册 robust list并退出。parent 验证 parent/child TID 写回、clear-child-tid、robust `OWNER_DIED` 和资源解除，输出 `thread futex ok`。QEMU `-smp 1/3/8` 均必须通过。

## 验证结果

`make verify` 已通过：格式、RISC-V workspace check、Clippy `-D warnings`、架构/接口围栏、三组件构建、ELF 静态检查、`git diff --check`，以及 QEMU `-smp 1/3/8` 冷启动均成功。
