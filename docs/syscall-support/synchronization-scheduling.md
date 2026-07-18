# Synchronization 与 scheduling syscall

| Number | Syscall | Status | 当前范围 |
|---:|---|---|---|
| 98 | `futex` | Partial | wait/wake/requeue、private/shared key 与 robust cleanup |
| 99 | `set_robust_list` | Complete | calling Thread registration 与 exit/exec cleanup |
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
| 283 | `membarrier` | Partial | query、private expedited register/execute |

## 已知缺口

futex PI、PI requeue、WAKE_OP、realtime scheduler classes 与跨 process expedited membarrier 尚未开放。
