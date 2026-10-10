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
| 74 | `signalfd4` | Complete | reader-relative private/shared dequeue、128-byte native records、read/readv、共享 mask、NONBLOCK/CLOEXEC、poll/epoll |
| 85 | `timerfd_create` | Complete | realtime、monotonic、boottime 与 NONBLOCK/CLOEXEC |
| 86 | `timerfd_settime` | Partial | relative/absolute、one-shot/periodic；无 CANCEL_ON_SET |
| 87 | `timerfd_gettime` | Complete | relative setting snapshot |

| 186 | `msgget` | Complete | IPC_PRIVATE、key/CREATE/EXCL、effective credentials、per-IPCNS ID/sequence |
| 187 | `msgctl` | Complete | RMID/SET/STAT、INFO、MSG_STAT/INFO/STAT_ANY，native generic 64-bit codec |
| 188 | `msgrcv` | Complete | FIFO/类型/负类型最小值、EXCEPT/NOERROR/COPY、blocking/EINTR/EIDRM |
| 189 | `msgsnd` | Complete | 一次 user-copy、blocking/NOWAIT、byte/message-count 上限、删除唤醒 |

| 190 | `semget` | Complete | per-IPCNS key/ID、数组与总量限额、effective credentials |
| 191 | `semctl` | Complete | GET/SET/STAT/INFO、等待计数、SET 清 undo、删除唤醒 |
| 192 | `semtimedop` | Complete | 与 semop 同一 operation、relative monotonic deadline、EAGAIN/EINTR、timeout 不回写 |
| 193 | `semop` | Complete | 多项/重复 index 原子回滚、zero/decrement 等待、NOWAIT、SEM_UNDO |
| 194 | `shmget` | Partial | per-IPCNS key/ID、实际共享页 backing、shmmax/shmall/shmmni；未配置 hugetlb hstate，HUGETLB 新建返回 EINVAL |
| 195 | `shmctl` | Partial | RMID 延迟销毁、SET/STAT、IPC_INFO/SHM_INFO、SHM_STAT/ANY，112-byte native64；实际 creator exit/shm_rmid_forced；LOCK/UNLOCK 的实际 UID/祖先配额与 unevictable backing |
| 196 | `shmat` | Complete | READONLY/RND/EXEC/REMAP、DAC、原 ID 对 DEST segment 再挂接、fork/split 实际 VMA nattch |
| 197 | `shmdt` | Complete | 原始挂接地址与同次 shmat 的 fragments，guard 外 fence/close |

新增 IPC namespace/消息队列/semaphore/SEM_UNDO 仍为源码实现，未集中编译或运行验证。上述范围描述实现目标，验证结果待最终门禁记录。

## 已知缺口

标准 semaphore 限额由 `/proc/sys/kernel/sem` 接入；

System V shared memory 包括 SHM_LOCK/UNLOCK，范围见下述 owner 契约。POSIX message queue、splice family 与 io_uring 尚未开放。
