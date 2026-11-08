# Synchronization 与 scheduling syscall

| Number | Syscall | Status | 当前范围 |
|---:|---|---|---|
| 98 | `futex` | Partial | wait/wake/requeue、WAKE_OP 源码、private/shared key 与 robust cleanup |
| 99 | `set_robust_list` | Complete | calling Thread registration 与 exit/exec cleanup |
| 100 | `get_robust_list` | Partial | namespace TID、REALCREDS/exec 串行化和原生 copyout 顺序源码 |
| 118 | `sched_setparam` | Partial | SCHED_OTHER priority validation |
| 119 | `sched_setscheduler` | Partial | SCHED_OTHER 与 reset-on-fork |
| 120 | `sched_getscheduler` | Complete | current stored policy |
| 121 | `sched_getparam` | Complete | current stored parameter |
| 122 | `sched_setaffinity` | Complete | logical online `CpuSet` |
| 123 | `sched_getaffinity` | Complete | logical affinity copyout |
| 124 | `sched_yield` | Complete | current runqueue transaction |
| 125 | `sched_get_priority_max` | Partial | 已开放 policy |
| 126 | `sched_get_priority_min` | Partial | 已开放 policy |
| 127 | `sched_rr_get_interval` | Partial | SCHED_OTHER timeslice projection |
| 140 | `setpriority` | Partial | process/thread nice scope |
| 141 | `getpriority` | Partial | process/thread nice scope |
| 274 | `sched_setattr` | Partial | OTHER/DEADLINE、版本化尺寸、KEEP_POLICY/PARAMS、CAP_SYS_NICE、CBS 准入 |
| 275 | `sched_getattr` | Partial | 静态配置与尺寸兼容，零 flags |
| 283 | `membarrier` | Partial | query、private expedited register/execute |

## 已知缺口

futex PI、PI requeue、FIFO/RR/BATCH/IDLE/EXT 设置与跨 process expedited membarrier 尚未开放。

sched_attr 为 56 字节（最小 v0 为 48）；set 的 size=0 按 v0 处理，超大/过短或非零未知尾部返回 E2BIG 并写回支持尺寸。
get 支持 48..=PAGE_SIZE，按实际复制长度填 size、未知尾部清零。copyin/copyout 失败 EFAULT，非零 syscall flags 非法。
支持 RESET_ON_FORK、KEEP_POLICY、KEEP_PARAMS；RECLAIM/DL_OVERRUN/util-clamp 返回 EOPNOTSUPP，未知位 EINVAL；动态 get flags 未开放。
DEADLINE 的 runtime 至少 1024 ns，0 < runtime <= deadline <= period，period=0 使用 deadline，周期范围按固定 Linux 默认为 100 us..4194304 us。
需要 initial UserNS CAP_SYS_NICE，目标 affinity 必须覆盖全部 active CPU；缩小 DEADLINE affinity 返回 EINVAL，准入不足 EBUSY。
原生采用分区 EDF 和 95% 的逐 CPU 密度上限，较 Linux 利用率准入保守；支持约束截止的 revised wakeup，不声明 GRUB 带宽回收或动态 overruns 通知。
显式 yield 放弃剩余周期预算。未设置 RESET_ON_FORK 的 DEADLINE fork/clone 返回 EAGAIN，设置后的子执行体恢复 OTHER；rr interval 为零。
proc stat 的 priority/policy 来自同一 Linux 配置快照；DEADLINE 显示 priority=-101、policy=6，不继续伪装为 OTHER。
scheduler-unit 的 CBS 准入/预算/唤醒用例、双架构 Clippy、musl syscall 与 2/4 CPU 公平性门禁通过；这些结果不等同于任意硬件的实时延迟保证。

get_robust_list 按当前 PID namespace 查找实际 Thread，在目标已有 Process credential_guard 下以 killable acquisition 与 exec 串行化；使用 caller real UID/GID 和 permitted capability 子集规则，FS 读取仍用 fsids/effective。授权后只复制 registration
scalar，先写 len 再写 head；不访问 head 指向的用户页。self/null/未映射 head、copyout 错误顺序以及跨进程 REAL/FS 区别的 guest 回归已补源码。此路径是冷查询，不增加调度或 futex 热路径锁。

未 reap zombie/退出 leader 使用查询时 pin 的实际 credential/MM handles 返回已清除的零 head；kernel executor 按 immutable credentials 授权并返回零 head。MM retirement 只保留退出 dumpability，nondumpable 的无 MM 目标需 initial UserNS SYS_PTRACE；
host 实际 membership/授权、guest dumpable/nondumpable zombie 与 reap 后 ESRCH 回归已补源码。

WAKE_OP 的五种实际 RMW、signed operands/compare、legacy shift、同/不同 key 双队列，private wake 无 VMA、legacy 零 wake、requeue signed counts/同 key 计数与 source/target 错误顺序已补源码。WAIT timespec 在 KTIME_MAX 饱和，monotonic absolute deadline
使用 calling Time namespace offset 转为 host。mandatory exit 复用既有预置 waiter，所有 scheduler handoff 在 guard 外进行。host/guest/benchmark/fence 覆盖实际 owner；timed WAIT/WAIT_BITSET 已接实际 Thread restart-block，
stop/continue 不重算期限；捕获 handler 始终 EINTR。PI 与 PI requeue 未开放，且不在本轮新增范围。
