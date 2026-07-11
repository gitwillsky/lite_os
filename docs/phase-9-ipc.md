# LiteOS Phase 9：IPC 能力边界

> 审计日期：2026-07-11（Asia/Shanghai）
> 代码基线：提交 `e4243f2`（Phase 0–8）
> 验证约束：不维护、不修正、不执行测试；只做生产代码静态检索、构建和非测试 QEMU 启动观察。

## 1. 阶段结论

Phase 9 确认当前生产代码没有 IPC 机制或 IPC syscall。Phase 1 已经删除：

- 私有 shared-memory handle/registry 与三个 SHM syscall；
- 私有 Unix-domain-socket path/listener 与 listen/accept/connect syscall；
- 私有 poll 与未接入的 ppoll 草稿；
- 匿名 pipe/FIFO 对象、自建 waiter 列表、创建/打开/读写 handler 及其用户程序。

本阶段不重新实现 `pipe2(2)`。Phase 8 刚确认当前没有 fd table 和 open file description；如果只为 pipe 增加一套 handle，将再次形成私有 IPC 模型。

## 2. 生产代码扫描

`kernel/src`、`user/src`、`syscall-abi/src` 中没有：

- `ipc/`、`pipe.rs`、`unix_socket.rs` 或 SHM 模块；
- `pipe2`、`ppoll`、`socket`、`bind`、`listen`、`accept4`、`connect` 或 SHM syscall number/dispatch/handler；
- pipe/socket/FIFO poll hook；
- IPC waiter、`Vec<Weak<Task>>` 唤醒列表或全表轮询；
- 用户 wrapper 或应用依赖。

仅剩两类字面命中：

1. ext2 将磁盘 inode mode `0x1000` 识别为 `InodeType::Fifo`。该枚举只用于类型识别，没有 FIFO 创建、打开或通信语义；ELF loader 只接受 `File`，因此 FIFO 不会被当作程序。
2. `DeadlineWaitQueue` 只以 scheduling entity 的 sleep deadline 为 key，由 Phase 6 调度状态机拥有。它不是通用 wait channel，不能冒充 pipe/futex/poll 等待队列。

## 3. 标准对照与决策

| 机制 | Linux/riscv64 入口 | 状态 | 决策 |
|---|---:|---|---|
| 匿名 pipe | `pipe2` 59 | Not Planned | 无 fd/OFD/close/dup/poll 闭环，不接入 |
| FIFO | `mknodat` 33 + `openat` 56 | Not Planned | 只识别磁盘类型，不提供通信对象 |
| futex | `futex` 98 | Removed | Phase 7 已明确无 shared/private key、timeout 和退出清理时不暴露 |
| 匿名共享映射 | `mmap` 222 + `MAP_SHARED|MAP_ANONYMOUS` | Missing | Phase 4 的 mmap 同名实现已删除，不使用私有 SHM handle 替代 |
| POSIX SHM | userspace `shm_open` + fd/filesystem | Not Planned | 无 tmpfs/`/dev/shm`/fd 模型 |
| Unix domain socket | socket syscall family | Not Planned | 无标准 socket fd、sockaddr 和 poll 闭环 |
| 等待多 fd | `ppoll` 73 | Not Planned | 无 pollable fd 对象与 signal-mask/EINTR 语义 |

`Not Planned` 表示当前标准内核基线不计划该能力，不表示编号可以返回成功；它们统一通过未知 syscall 路径返回 `-ENOSYS`。

## 4. pipe2 恢复门槛

未来只有同时满足以下条件才能恢复 pipe：

1. Process 拥有正确的 fd table，fd entry 的 `FD_CLOEXEC` 与 OFD status flags 分离。
2. `pipe2(flags)` 一次分配两个 fd，copyout 失败可完整回滚，flags 只接受 Linux 允许子集。
3. `close`、`dup/dup3`、fork 共享 OFD 和端点引用计数正确。
4. 读端空/写端满通过唯一 wait queue 阻塞，wake-before-sleep 无丢失唤醒，不轮询。
5. 支持 partial read/write、EOF、`EPIPE`/SIGPIPE 决策、`O_NONBLOCK`、`EINTR` 和 poll readiness。

当前不具备第 1、3、4、5 项，因此实现 pipe2 会扩展为 fd、signal 和 poll 的多阶段重建，超出本阶段最小正确边界。

## 5. 不变量

- IPC 能力的唯一权威状态是“未支持”；没有内核 registry、handle 或不可达对象与该结论冲突。
- syscall 表不包含 IPC 编号；不存在私有编号、错号转发或同名近似实现。
- Phase 6 deadline queue 只服务 nanosleep；未经完整 key/lifetime/wakeup 设计，不扩展为装饰性通用 wait queue。

## 6. 验证结果

- 静态扫描全部生产源码：无 pipe/SHM/UDS/poll 模块、syscall、wrapper、waiter 或用户依赖。
- `cargo check --workspace`：通过；kernel 258 个既有 warning，IPC 路径无 warning。
- `make build-user`、`make build-kernel`、`make build-bootloader` 与 ext2 镜像重建：通过。
- 两轮 8-hart QEMU 冷启动：ext2 挂载、init 创建/入队，观察窗口内无 panic/fault。Phase 9 未改变生产代码，因此复用同一生产树的 Phase 8 构建与启动证据。
- 按仓库规则未执行、维护或修正测试用例。

## 7. 剩余风险与 Phase 10

- 当前没有 IPC 功能可用；这是明确的支持边界，不是已实现能力。
- 过时的项目概述文档仍包含旧 IPC/FAT32/完整 Linux 兼容声明；Phase 13 将统一重写 README/架构文档，不在本阶段零散维护两套概述。
- Phase 10 将审计 device/bus/IRQ/DMA，优先删除无标准用户接口或不在启动路径的设备与管理抽象。
