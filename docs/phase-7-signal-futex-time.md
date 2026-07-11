# LiteOS Phase 7：信号、futex 与时间

> 审计日期：2026-07-11（Asia/Shanghai）
> 代码基线：提交 `1b46ca2`（Phase 0–6）

## 1. 能力结论

- 改动前暴露 `kill(2)` 与 `rt_sigreturn(2)`，但没有 `rt_sigaction`/`rt_sigprocmask`，signal frame 是私有布局，handler 依赖取指地址 0 的特殊 page fault 返回，kill 只接受正整数 PID。该组合不满足 Linux/riscv64 ABI。
- 代码库没有 futex syscall、用户 ABI 或 waiter key 实现。Phase 6 deadline queue 不能冒充 futex queue。
- `nanosleep(2)` 使用 monotonic deadline，但 timespec 使用无符号字段、不输出 `rem`；没有 `clock_gettime(2)`。realtime offset 只保留整秒，丢失 RTC 亚秒精度。

## 2. 决策

1. 删除不完整的 kill/rt_sigreturn syscall number、dispatcher、signal subsystem、TCB signal state、Stopped 状态和地址 0 sigreturn trap。非法指令、断点与 page fault 继续明确终止当前 Process。
2. 不暴露 futex；没有标准 wait/wake/timeout/共享 key/退出清理前，返回 ENOSYS 比同名半实现正确。
3. 保留 Linux `nanosleep` 101，timespec 改为两个 i64，严格校验负值、nsec 范围与 overflow，并在 EINTR 时 copyout `rem`。
4. 增加 Linux `clock_gettime` 113，只支持 `CLOCK_REALTIME` 与 `CLOCK_MONOTONIC`；其他 clock ID 返回 EINVAL。
5. realtime 保存纳秒级 `RTC_now - monotonic_now` offset；monotonic 始终来自 DTB timebase/mtime，不受 RTC 调整影响。

## 3. 明确边界

- 当前不是“支持部分信号”，而是明确不提供 signal syscall。未来恢复信号时必须一次性提供标准 rt_sigaction、rt_sigprocmask、riscv64 rt frame/trampoline、pending selection 与 syscall interruption。
- 当前不是“支持 private futex”，而是无 futex ABI。未来实现必须复用 Phase 6 lost-wakeup 状态机，并以 address-space identity + aligned user address 为 key。
- realtime fallback 仅在 RTC 缺失时使用固定 2024-01-01 epoch offset；不会伪造 RTC 精度。

## 4. 验收条件

- syscall ABI 与生产源码不存在 kill/rt_sigreturn/custom signal frame/futex 假实现。
- clock_gettime 两种 clock 的 timespec 正规化为 `0 <= tv_nsec < 1e9`。
- nanosleep 拒绝负值、非法 nsec 和算术 overflow；用户 copyin/copyout fault 返回 EFAULT。
- 构建与 8-hart QEMU 冷启动通过；不运行测试。

## 5. 验证结果

- `git diff --check`、`cargo check --workspace`：通过；kernel warning 从 306 降至 291。
- 三组件构建通过；8-hart QEMU（boot hart 0）成功挂载 ext2、创建 init，观察窗口内无 panic/fault。
- static search：syscall ABI 与 kernel 不再包含 kill、rt_sigreturn、signal frame、Stopped state 或 futex 同名实现。
- `clock_gettime` 输出始终正规化；nanosleep 输入使用 i64 校验并对乘加 overflow 返回 EINVAL。
- 按仓库规则未执行、维护或修正测试用例。
