# LiteOS 文档索引

本文件是仓库文档的唯一总索引。每个事实只由下列一个文档 owner 维护；其他文档只链接，不复制。

## 架构

- [当前架构总则](architecture.md)
- [启动与平台](architecture/boot-platform.md)
- [执行、CPU、trap、timer 与同步](architecture/execution.md)
- [内存](architecture/memory.md)
- [进程与调度](architecture/process-scheduling.md)
- [文件系统与存储](architecture/filesystem-storage.md)
- [IPC 与网络](architecture/ipc-network.md)
- [设备与终端](architecture/devices-terminal.md)
- [用户态与 ABI](architecture/userspace-abi.md)

## 架构契约

- [全局 module、依赖与接口契约](architecture-contract.md)
- [启动与平台契约](architecture-contract/boot-platform.md)
- [执行域契约](architecture-contract/execution.md)
- [内存契约](architecture-contract/memory.md)
- [进程与调度契约](architecture-contract/process-scheduling.md)
- [文件系统与存储契约](architecture-contract/filesystem-storage.md)
- [IPC 与网络契约](architecture-contract/ipc-network.md)
- [设备与终端契约](architecture-contract/devices-terminal.md)
- [用户态与 ABI 契约](architecture-contract/userspace-abi.md)

## Linux/riscv64 ABI

- [syscall 支持总则](syscall-support.md)
- [进程与身份](syscall-support/process-identity.md)
- [内存](syscall-support/memory.md)
- [文件系统与 I/O](syscall-support/filesystem-io.md)
- [同步与调度](syscall-support/synchronization-scheduling.md)
- [信号与时间](syscall-support/signal-time.md)
- [IPC](syscall-support/ipc.md)
- [Socket](syscall-support/socket.md)
- [系统](syscall-support/system.md)

## 工程基线

- [固定规范与上游来源](standards-baseline.md)
- [构建、测试与验证](development/build-and-verify.md)
- [生成的 scoped interface baseline](generated/architecture-interface.txt)

完成的计划、阶段快照和已被当前文档吸收的设计草案不保留在工作树中；需要追溯时使用 Git 历史。
