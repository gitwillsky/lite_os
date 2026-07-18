# Signal 与 time syscall

| Number | Syscall | Status | 当前范围 |
|---:|---|---|---|
| 101 | `nanosleep` | Complete | interrupt、remaining time 与 restart record |
| 102 | `getitimer` | Complete | ITIMER_REAL |
| 103 | `setitimer` | Complete | ITIMER_REAL phase 与 replacement |
| 107 | `timer_create` | Partial | supported clocks 与 signal notification |
| 108 | `timer_gettime` | Complete | POSIX timer snapshot |
| 109 | `timer_getoverrun` | Complete | bounded overrun projection |
| 110 | `timer_settime` | Complete | absolute/relative deadline |
| 111 | `timer_delete` | Complete | owner index cleanup |
| 113 | `clock_gettime` | Partial | realtime、monotonic 与 process/thread CPU clocks |
| 114 | `clock_getres` | Partial | supported clocks |
| 115 | `clock_nanosleep` | Partial | supported clocks、absolute/relative wait |
| 129 | `kill` | Partial | PID/group selectors、permission 与 signal zero |
| 130 | `tkill` | Complete | Thread-directed generation |
| 131 | `tgkill` | Complete | TGID/TID validation |
| 132 | `sigaltstack` | Complete | registration、active projection、autodisarm |
| 133 | `rt_sigsuspend` | Complete | atomic mask/wait transaction |
| 134 | `rt_sigaction` | Complete | disposition、mask 与 supported flags |
| 135 | `rt_sigprocmask` | Complete | per-Thread mask |
| 137 | `rt_sigtimedwait` | Partial | standard signal set；无 queued realtime payload |
| 139 | `rt_sigreturn` | Complete | RV64 frame restore 与 syscall replay |
| 169 | `gettimeofday` | Complete | realtime snapshot |

## 已知缺口

queued realtime signal、全部 restartable syscall、其他 POSIX clock/timer notification mode 与非 RV64 signal-frame backend 尚未开放。
