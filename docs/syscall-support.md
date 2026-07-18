# Linux/riscv64 syscall 支持

LiteOS dispatcher 当前接入 146 个 Linux/riscv64 syscall。该数量只由 `syscall-abi/src/lib.rs` 和本页维护；每个入口的状态、对象范围与缺口只在一个领域矩阵中出现。

## ABI 总则

- 编号、寄存器调用约定、UAPI layout/flags、负 errno 与 restart 语义以 [固定 Linux revision](standards-baseline.md) 为准。
- dispatcher 只使用共享 `SYSCALL_*` 常量；raw numeric arm、私有编号、错号转发和兼容入口禁止。
- syscall handler 只负责编解码、user-copy、errno 与领域 façade 调用，不拥有 process、memory、file、socket 或 device state。
- 未接入的 number 返回 `ENOSYS`，不得逐调用打印或伪造成功。
- `Complete` 表示当前表中声明的对象/flag/并发范围完整；不外推到未声明对象。`Partial` 必须明确已开放范围和缺口。

## 领域矩阵

| 领域 | 唯一矩阵 |
|---|---|
| Process、credential 与 identity | [process-identity](syscall-support/process-identity.md) |
| Virtual memory | [memory](syscall-support/memory.md) |
| Filesystem 与 I/O | [filesystem-io](syscall-support/filesystem-io.md) |
| Futex、scheduler 与 memory barrier | [synchronization-scheduling](syscall-support/synchronization-scheduling.md) |
| Signal 与 time | [signal-time](syscall-support/signal-time.md) |
| Pipe、eventfd 与 multiplexing | [ipc](syscall-support/ipc.md) |
| Socket | [socket](syscall-support/socket.md) |
| System | [system](syscall-support/system.md) |

## 全局已知缺口

当前矩阵不声明 futex PI/PI-requeue/WAKE_OP、完整 clone flags、所有 syscall restart、queued realtime signal、IPv6、多 interface/network namespace、完整 DRM/evdev UAPI、swap 或后台 reclaim/writeback。
固定 musl、BusyBox 与 APK consumer gate 证明的是矩阵列出的 vertical slice，不是完整 Linux、POSIX 或任意 musl compatibility。
