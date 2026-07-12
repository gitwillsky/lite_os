# Phase 38：Process/session/signal 生命周期闭环

## 目标

按固定 Linux v7.1 `kernel/signal.c`、`kernel/exit.c`、`kernel/sys.c` 与 POSIX.1-2024 `_Exit()` 语义闭合 global init、exec/setpgid、orphan process group 和 controlling terminal 退出路径。实现不引入 credentials 草稿，不复制 SID/PGID/parent/stopped 状态。

## 唯一 owner 与 seam

1. TaskManager process graph 唯一保存 parent、SID、PGID、`has_execed` 与 job-control state；
2. child exec 在新映像完整准备后、不可失败提交前发布 `has_execed`，与 parent `setpgid` 通过同一 graph lock 排序；
3. Process 退出前后在 graph 内比较 orphaned+stopped group，冻结 transition 当时的 TGID 集合；
4. Terminal 只原子交出 exiting session 的 foreground PGID，graph 在相同临界区冻结该 group 的 live TGID；
5. graph lock 释放后，所有 SIGHUP/SIGCONT 继续复用现有 kernel signal generation、pending、wake 与 scheduler continue seam。

固定锁序为 process graph -> Terminal。Terminal input/TTY syscall 路径不会持 Terminal lock 进入 graph；缺失该顺序会使 session exit 与 `TIOCSPGRP` 并发时形成锁环，或按变化后的 PGID 错发 SIGHUP。

## 语义

- global PID 1 对默认 disposition signal 保持 unkillable；显式 handler 仍接收，blocked signal 可保持 pending，SIGKILL/SIGSTOP 不因 mask 进入 pending。同步 fault 直接走强制退出路径。
- child 成功进入 exec point-of-no-return 后，parent 再调用 `setpgid(child, ...)` 返回 `EACCES`；失败的 exec 不发布该状态。
- 仅 Process exit/reparent 导致的 newly orphaned group 在包含 stopped Process 时收到全组 SIGHUP，随后全组 SIGCONT；`setpgid/setsid` 不扩展触发该序列。
- 默认 SIGTSTP/SIGTTIN/SIGTTOU 在 delivery 时发现 caller group 已 orphaned 就丢弃，SIGSTOP 始终可停止。
- controlling process 退出时，原 foreground group 收到 SIGHUP，Terminal 与旧 session 解除关联，后续 session leader 可重新取得它。

## 架构收敛

- session/PGID/exec/orphan policy 从巨型 `task_manager.rs` 提取到 `task_manager/process_group.rs`，主文件降至 1311 行并重新建立只降不升的额度；
- exec 映像提交从 `model.rs` 提取到 `model/process_exec.rs`，主文件降至 1099 行；
- 新模块只暴露既有 process-group façade 与两个 task-internal query/commit seam，没有第二套 graph 或 signal adapter。

## 验证

动态 BusyBox gate 使用真实 `init + ash` 验证：

- `kill -KILL 1` 返回后 init 仍存在；
- background child 停止后退出 controlling shell，child 被 reparent 导致 group orphan，依次观察 SIGHUP handler 与 SIGCONT 后续执行；
- init respawn 的新 shell 能重新取得同一 controlling Terminal；
- foreground child 终止 controlling shell 后收到 session-exit SIGHUP，随后 init 再次恢复 console shell。

固定 musl consumer 额外使用继承 pipe 建立确定顺序：child 完成 `execve` 并从新映像通知 parent 后，parent 的 `setpgid(child, child)` 必须返回 `EACCES`，随后以 SIGKILL 回收 child。

`make verify` 仍是唯一强制入口，覆盖架构围栏、Clippy、构建、1/3/8 hart、musl 与动态 BusyBox 冷启动。
