# LiteOS Architecture Contract

本文只定义稳定的 module interface、依赖和状态 owner。当前功能事实属于 `architecture.md`，Linux ABI 状态属于 `syscall-support.md`。

## 1. Crate contract

- `bootloader` 是独立 M-mode domain，不依赖 kernel 或 user。
- `syscall-abi` 只保存用户可见 Linux/riscv64 ABI 常量，不依赖实现 crate。
- `user` 只依赖 `syscall-abi` 与自身 runtime。
- kernel 的 `main.rs` 是唯一 composition root；初始化顺序和 adapter 装配不得下沉到 driver、filesystem 或 task。

## 2. Kernel dependency contract

| Module | 允许依赖 | 禁止依赖 |
|---|---|---|
| `arch` | config、memory constants、sync mechanism | task、fs、drivers、syscall、trap |
| `sync` | arch interrupt mechanism | task、fs、drivers、syscall、trap |
| `memory` | arch、config、id、sync | task、fs、drivers、syscall、trap |
| `drivers` | arch、memory、sync | task、fs、syscall、trap |
| `fs` | block interface、timer、sync | task、syscall、trap、具体 device adapter |
| `task` | arch mechanism、memory、fs interface、sync、timer | drivers、syscall、trap |
| `trap` | arch、drivers interrupt interface、task、syscall、timer | filesystem implementation |
| `syscall` | task、VFS、memory interface、timer | arch、drivers、trap、ext2、scheduler/page-table implementation |

同一 module 内引用不构成跨 seam 依赖。`main.rs` 可以依赖所有 kernel module，但只能做装配、启动顺序和 fail-stop 策略。

## 3. State owners

| 状态 | 唯一 owner |
|---|---|
| M-mode initialization、HSM、RFENCE | bootloader 对应 module |
| DTB board facts | immutable BoardInfo publication |
| hart possible/online/active、startup stack | HartTopology |
| per-hart current、runqueue、mailbox | task ProcessorTopology |
| task run state、generation、wait membership | SchedulingState |
| process address space、cwd、fd table | Process |
| OFD offset/status flags | OpenFileDescription |
| virtual mappings | MemorySet/PageTable owner |
| physical frame lifetime | FrameTracker/frame allocator |
| root mount、pathname traversal | VFS |
| inode/on-disk allocation state | filesystem adapter mutation domain |
| VirtIO descriptor/DMA lifetime | VirtQueue/driver instance |
| interrupt registration/affinity | interrupt controller |
| syscall number | syscall-abi |
| user pointer与 errno translation | syscall module |

新增 global、Atomic、lock、cache 或 flag 必须在声明附近写 `OWNER:`，并说明缺失该 owner 会造成的具体状态分裂。

## 4. Interface and capability contract

- kernel 与 bootloader 是 binary crate，跨 module interface 使用 `pub(crate)`；不得使用裸 `pub` 伪造外部 interface。
- 默认 private；`pub(crate)` surface 由 `architecture-interface.txt` 完整记录。
- filesystem 只能看到 `drivers::block` seam，不得看到 VirtIO adapter。
- MMIO/volatile 只存在于 arch/driver HAL；user pointer 只通过 AddressSpace copy；磁盘 packed layout 只存在于 filesystem adapter。
- raw CSR、DMA、page-table pointer、trap context 和 packed disk unsafe 必须有局部 `SAFETY:` 证明。
- 禁止 `static mut`、私有 syscall、固定 hart 容量、console syscall 旁路、deprecated/feature-flag 双轨。
- 禁止 `common/utils/helpers/misc/manager/base/shared/core` 等无领域含义的目录。

## 5. Change contract

修改前必须确定所属 module、状态 owner、interface 变化、依赖变化以及 error/exit/interrupt cleanup。扩大 interface 或新增 global state 必须更新对应权威契约；不得修改围栏来掩盖实现问题。

唯一强制验证入口是 `make verify`。
