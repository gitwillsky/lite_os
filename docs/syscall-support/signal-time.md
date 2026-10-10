# Signal 与 time syscall

| Number | Syscall | Status | 当前范围 |
|---:|---|---|---|
| 101 | `nanosleep` | Complete | interrupt、remaining time 与 restart record |
| 102 | `getitimer` | Complete | ITIMER_REAL |
| 103 | `setitimer` | Complete | ITIMER_REAL phase 与 replacement |
| 107 | `timer_create` | Partial | REALTIME/MONOTONIC/BOOTTIME、creator preallocated quota、实际 PID identity target；新增生命周期源码待集中验证 |
| 108 | `timer_gettime` | Complete | POSIX timer snapshot |
| 109 | `timer_getoverrun` | Complete | bounded overrun projection |
| 110 | `timer_settime` | Complete | absolute/relative deadline |
| 111 | `timer_delete` | Complete | owner index cleanup |
| 113 | `clock_gettime` | Partial | realtime、monotonic、monotonic raw、realtime/monotonic coarse、boottime 与 process/thread CPU clocks |
| 114 | `clock_getres` | Partial | 上述 supported clocks 的实际 counter/runtime 分辨率 |
| 115 | `clock_nanosleep` | Partial | REALTIME/MONOTONIC/BOOTTIME、namespace absolute 与 host relative wait；CPU clocks EOPNOTSUPP |
| 128 | `restart_syscall` | Partial | Thread 实际 Futex/relative Sleep restart block、冻结 deadline；新增源码待集中验证 |
| 129 | `kill` | Partial | PID/group selectors、permission 与 signal zero |
| 130 | `tkill` | Complete | Thread-directed generation |
| 131 | `tgkill` | Complete | TGID/TID validation |
| 132 | `sigaltstack` | Complete | registration、active projection、autodisarm；AArch64/RV64 `MINSIGSTKSZ` 分别为 5120/2048 |
| 133 | `rt_sigsuspend` | Complete | atomic mask/wait transaction |
| 134 | `rt_sigaction` | Complete | 跨 Process SIGHAND sharing、ignore pending discard、exec detach、blocked ignored arrival、NOCLDSTOP/NOCLDWAIT、mask 与标准未知 flag 探测 |
| 135 | `rt_sigprocmask` | Complete | per-Thread mask |
| 136 | `rt_sigpending` | Partial | blocked private/shared union、0..8-byte native output；新增源码待集中验证 |
| 137 | `rt_sigtimedwait` | Partial | 同一 pending owner 的普通/实时 FIFO 消费、独立 siginfo、KTIME_MAX deadline；新增源码待集中验证 |
| 138 | `rt_sigqueueinfo` | Partial | 标准 LP64 native siginfo、来源约束、UserNS 投影与 recipient RT quota；新增源码待集中验证 |
| 240 | `rt_tgsigqueueinfo` | Partial | 精确 TGID/TID、同一 DAC/queue owner；新增源码待集中验证 |
| 139 | `rt_sigreturn` | Complete | architecture-owned frame、寄存器恢复与 syscall replay；坏 frame 强制 SIGSEGV（RV SI_KERNEL，AArch SP 地址错误码） |
| 169 | `gettimeofday` | Complete | realtime snapshot |

## 已知缺口

Time namespace 的 MONOTONIC/RAW/COARSE 和 BOOTTIME 分别投影实际 owner 的两个 offset；
absolute sleep、timerfd/POSIX timer_settime 在 replacement 时使用调用者 namespace，
relative duration、realtime 和 CPU clocks 保持原 domain。原始非零 deadline 转 host 零仍为
已到期的 armed timer。当前 host 没有 suspend/NTP，clock 为同源 counter；没有 userspace
clock vDSO 快路径。Time namespace 回归归属下述集中运行门禁。
合法 timespec/timeval 的 ns 转换与 relative deadline 在 KTIME_MAX 饱和，不能因乘加溢出
把合法的大时间误报 EINVAL；timerfd/POSIX/nanosleep 共享同一 timespec 转换。

普通/实时 FIFO、native sigqueue ucounts 和 user queued 入口使用同一 signal owner；
未声明的 restart-block、POSIX clock/timer notification mode 不支持。普通 queue
nonnegative source 可越过 soft/ancestor maximum 但仍收费；负 code 无 record 时按 native
bit-only SI_USER fallback 继续，SIGKILL 无 record。SIGCHLD 的准备在 graph 外，最终 mask/
disposition/pending 与 autoreap 保持原事务，父/creator/reaper 改边则复查重试。
现有 POSIX signal timers 在创建时预留一个实际通知（SIGEV_NONE 同样占创建 quota），后续降限
不阻止到期；同一 timer 不累积多个 pending record，周期重装与 overrun 在 handler/sigwait
消费时提交。timer_settime 使旧 sequence 失效，删除/ID 复用按实际 Arc 拒绝旧通知；exec
显式清旧 SI_TIMER，SIG_IGN 停放周期通知、恢复后发布，SIGEV_THREAD_ID 输入按实际 PID namespace
投影且绑定原 identity。相关 producer/guest/benchmark 覆盖见运行时回归文档，普通 pending
与 timer 共用 signal 的 ucounts/合并交互使用同一预算 owner。

AArch64 `rt_sigframe` 固定为 4688 bytes，只接受一个 528-byte `FPSIMD_MAGIC`
record 和 null terminator；SVE/SME、ESR、extra context 及动态扩展 frame 尚未开放。
`rt_sigreturn` 只允许恢复 NZCV，拒绝 DAIF、PAN 等未由用户 ABI owner 管理的 PSTATE 位，
并要求 PC 位于用户地址空间且 4-byte 对齐。

计时 futex 与相对 nanosleep/clock_nanosleep 已接标准 restart-block；无 handler 的 stop/continue 保留原期限，捕获 handler 始终 EINTR，rt_sigreturn 清理 block。绝对 clock_nanosleep 使用 ERESTARTNOHAND，STOP 不提前消费重放记录；
guest 回归和实际 owner benchmark 覆盖这些行为。
