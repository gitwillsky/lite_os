# Phase 58：Anonymous shared memory 与 non-PI futex

本阶段固定对照 Linux v7.1 futex/mmap UAPI、POSIX.1-2024 process-shared synchronization 与 musl v1.2.6，实现一条 `MemorySet shared backing → FutexKey → IndexedWaitQueue → musl pthread` 竖切。不存在私有共享 handle、第二套 wait queue 或 libc patch。

## Owner 与 interface

- `MemorySet` VMA 表继续唯一拥有映射。每次 anonymous `MAP_SHARED` 创建一个 Arc backing，backing 唯一持有清零 frame 集合；fork clone 复用 backing/frame，private VMA 继续走既有 COW。
- memory module 的单调 backing ID 只解决 futex scalar key 的 ABA：若使用可复用 Arc 地址，旧 mapping 在 waiter 存活期间 munmap 后，新 mapping 可能错误唤醒旧 waiter。ID 耗尽 fail-stop，不维护可漂移的 global registry。
- `MemorySet` 在 AddressSpace lock 内把 private futex 归一化为 `(AddressSpace identity,uaddr)`，把 anonymous/file shared futex 归一化为 `(backing/file identity,offset)`。TaskManager 不读取 VMA、frame 或 page-cache adapter。
- `IndexedWaitQueue` 仍是唯一 registration owner。waiter 的 key、bitset、deadline 与 wait ID 同 entry 保存；requeue 只原地替换 key，signal、timeout 与 wake 继续通过同一 remove 路径消费全部 indexes。

## Linux ABI

- `mmap(222)` 接受 `MAP_SHARED|MAP_ANONYMOUS`，要求 `fd=-1,offset=0`，支持 hint、fixed variants、`PROT_NONE` 与 W^X。
- `futex(98)` 完成 non-PI `WAIT/WAKE`、`REQUEUE/CMP_REQUEUE`、`WAIT_BITSET/WAKE_BITSET` 与 PRIVATE flag。
- `WAIT` timespec 是 relative monotonic；`WAIT_BITSET` timespec 是 absolute monotonic，附加 `FUTEX_CLOCK_REALTIME` 时由 timer owner 转换为 registry 使用的 monotonic deadline。零 bitset 返回 `EINVAL`，compare mismatch 返回 `EAGAIN`。
- PI、PI-requeue 与 WAKE_OP 未建立调度优先级捐赠/原子 user operation owner，本阶段继续返回 `ENOSYS`，不保留同名占位实现。

## 固定 consumer

`scripts/fixtures/musl-shared-sync.c` 作为独立 verification translation unit 链入唯一静态 musl smoke，避免继续扩大主 consumer 或增加第二个 userspace 入口。它在一个 anonymous shared page 中完成：

1. fork 父子通过 `PTHREAD_PROCESS_SHARED` mutex/condition 与 process-shared semaphore 同步，并核对共享写入；
2. raw `WAIT_BITSET|CLOCK_REALTIME` waiter 由 `CMP_REQUEUE` 返回值完成无时间假设的注册握手，迁移到第二个 word 后以不相交 mask 证明不会被误唤醒，再由匹配 mask 精确 wake；
3. 验证 zero bitset、past absolute timeout、compare mismatch 与普通 REQUEUE 空队列结果，最后销毁同步对象并 munmap。

`make verify` 是唯一提交门：执行架构/interface 围栏、RISC-V Clippy/构建、ELF 检查与固定 musl/BusyBox/拓扑 runtime gate。Phase 58 不把固定 consumer 外推为完整 pthread 或任意 musl 程序兼容。
