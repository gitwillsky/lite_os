# IPC 与 I/O multiplexing syscall

| Number | Syscall | Status | 当前范围 |
|---:|---|---|---|
| 19 | `eventfd2` | Complete | counter/semaphore、blocking、每次成功读写的 poll edge |
| 20 | `epoll_create1` | Complete | CLOEXEC |
| 21 | `epoll_ctl` | Partial | ADD/MOD/DEL、ET/ONESHOT/EXCLUSIVE 与 bounded nesting |
| 22 | `epoll_pwait` | Complete | signal-mask atomic wait |
| 59 | `pipe2` | Complete | byte ring、PIPE_BUF、nonblock/CLOEXEC |
| 72 | `pselect6` | Complete | fd readiness、deadline 与 signal mask |
| 73 | `ppoll` | Complete | fd readiness、deadline 与 signal mask |
| 85 | `timerfd_create` | Complete | realtime、monotonic、boottime 与 NONBLOCK/CLOEXEC |
| 86 | `timerfd_settime` | Partial | relative/absolute、one-shot/periodic；无 CANCEL_ON_SET |
| 87 | `timerfd_gettime` | Complete | relative setting snapshot |

## 已知缺口

System V IPC、POSIX message queue、signalfd、splice family 与 io_uring 尚未开放。
