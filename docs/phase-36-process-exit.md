# Phase 36：Thread Group 退出事务

## 目标

删除普通退出码与 signal death 的双轨编码，使 `exit`、`exit_group` 和默认致命 signal 遵循固定 Linux v7.1 进程生命周期语义，并由真实 musl pthread consumer 验证。

## 唯一模型

Process graph 是 group-exit status 与 live Thread collection 的唯一 owner。`exit` 只注销 calling Thread；首个 `exit_group` 或默认致命 signal 原子固定退出原因，后续并发发起者不得覆盖。发起者向所有 sibling 注入不可屏蔽终止唤醒并请求远端 CPU 调度，但不远程释放 TCB 或 kernel stack。

每个 Thread 回到自己的 trap/内核栈后，统一执行：

1. robust-list owner-death cleanup；
2. 从 process graph 注销；
3. clear-child-tid 写零与 futex wake；
4. 非最后 Thread 移除独立 trap context；
5. 最后一个 Thread 关闭共享 fd table、发布 zombie、唤醒 parent 并发送 SIGCHLD。

Process zombie 直接保存领域状态 `Exited(code)` 或 `Signaled(signal)`。`wait4` 只在 ABI 边界编码一次；SIGCHLD 分别产生 `CLD_EXITED` 或 `CLD_KILLED`，不再把 signal death 保存为 `128 + signal` 的 shell 展示码。

## 验证

固定 musl consumer 新增两个真实多线程 child：

- worker 存活时由主线程调用 musl `_exit(42)`，parent 同时验证 `CLD_EXITED/si_status=42` 与 `WIFEXITED/WEXITSTATUS=42`；
- worker 存活时执行 `kill(getpid(), SIGTERM)`，parent 同时验证 `CLD_KILLED/si_status=SIGTERM` 与 `WIFSIGNALED/WTERMSIG=SIGTERM`。

BusyBox foreground Ctrl-C gate 在 ash 返回新提示符后注入独立存活探针。signal 正确终止 foreground job 时，ash 会中止包含 `fg` 的当前 command list；继续要求同一 list 的后续 `echo` 执行会反向依赖旧的普通 exit-code 双轨。

`make verify` 继续作为唯一强制入口，覆盖架构围栏、workspace check/clippy、构建、ELF 静态检查、1/3/8 hart 启动、固定 musl consumer 和动态 BusyBox gate。
