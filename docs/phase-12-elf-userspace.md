# LiteOS Phase 12：静态 ELF、exec 与最小用户态

> 审计日期：2026-07-11（Asia/Shanghai）  
> 代码基线：提交 `ef6b48f`（Phase 0–11）  
> 规范基线：[standards-baseline.md](standards-baseline.md) 固定的 Linux `v7.1`、RISC-V ELF psABI `e03d44ae`与 musl `v1.2.6`  
> 验证约束：不维护、不修正、不执行测试；只做静态 ELF/ABI 检查、构建和非测试 QEMU 启动观察。

## 1. 阶段结论

Phase 12 将当前用户态收缩为一条可证明的静态 RV64 路径：

`ext2 executable inode -> full file read -> validated ET_EXEC/PT_LOAD -> new MemorySet -> Linux initial stack -> _start -> main -> Linux syscall`

本阶段不声称支持 PIE、动态链接、TLS 或常规 musl 程序。它证明的子集是：

- ELF64、little-endian、RISC-V、静态 `ET_EXEC`；
- eager `PT_LOAD`，BSS 补零，每段权限来自 ELF，严格 W^X；
- Linux 格式 `argc/argv/envp/auxv` 初始栈，`sp` 16-byte aligned；
- `AT_PHDR/AT_PHENT/AT_PHNUM/AT_PAGESZ/AT_ENTRY`；
- path/argv/envp 按 NUL-terminated bytes 处理，不再要求 UTF-8；
- `execve` 失败在新 AddressSpace 完整准备前不修改当前 Process。

## 2. 改动前已确认的问题

| 严重度 | 问题 | 被破坏的契约 | 后果 |
|---|---|---|---|
| Blocker | 链接脚本的第一个 `PT_LOAD` 从 file offset `0x1000` 开始 | `AT_PHDR` 必须指向用户地址空间内的 program header table | ELF/program headers 完全没有被映射，无法构造有效 auxv |
| Blocker | 初始栈只有 `argc/argv/envp` | Linux process startup 需要 NULL-terminated auxv | libc/crt 无法获取 page size、PHDR 与 entry |
| Critical | path/argv/envp 全程使用 Rust `String` | Linux pathname/argument ABI 是任意非 NUL bytes | 非 UTF-8 参数错误返回 `EINVAL` |
| Critical | loader 用 `Option<Vec<u8>>` 折叠所有错误 | `execve` 必须区分 pathname、I/O、format 与 OOM | I/O/short read/坏 ELF 都可被报成 `ENOENT` 或 `EINVAL` |
| Major | init 的初始栈是 `argc=0` | kernel-created init 应有稳定 `argv[0]` | 自带 runtime 与真实 exec startup 路径不同 |
| Major | `_start` 不读初始栈、不初始化 `gp` | RISC-V psABI process entry / small-data 基准 | main 无法接收 argc/argv/envp，未来 `.sdata` 会使用无效 `gp` |
| Critical | loader 只检查 regular inode，忽略 execute mode | root 也必须看到至少一个 execute bit | 非可执行 mode 的文件可以被启动 |
| Major | 用户库保留 657 行自定义堆与 116 行 test utility | 当前 init 不分配，仓库规则又禁止维护/执行测试 | 无关用户策略和孤儿测试框架扩大信任面 |

## 3. 当前静态 ELF 契约

### 3.1 ELF header

loader 只接受：

- ELF magic 正确；
- ELF64；
- little-endian；
- `EI_VERSION = EV_CURRENT` 且 ELF header version 为 1；
- `e_ehsize = 64`，`e_phentsize = 56`，program header table 完整在 file 内；
- `EM_RISCV`；
- `ET_EXEC`；
- RISC-V flags 只允许 RVC 和 soft/single/double-float ABI。

RV32E flag、quad-float ABI、TSO flag 和未知 flag 均拒绝，因为当前没有对应执行环境。`ET_DYN`、`PT_DYNAMIC`、`PT_INTERP` 与 `PT_TLS` 都返回 `ENOEXEC`，不保留未完成的动态 loader 或 TLS fallback。

### 3.2 Program headers 与 LOAD

每个非空 `PT_LOAD` 必须同时满足：

1. `p_filesz <= p_memsz`；file/virtual range 都经过 checked arithmetic，且完整落在 file 与 Sv39 用户低半区。
2. `p_align` 为 0/1 或 2 的幂，并满足 `p_vaddr % p_align == p_offset % p_align`。
3. 权限只从 `PF_R/PF_W/PF_X` 构造；任何 W+X segment 直接拒绝。
4. file bytes 只复制到 `p_filesz`；`p_memsz - p_filesz` 由零化 FrameTracker 提供 BSS 零值。
5. ELF entry 必须落在 `U|X` leaf；program header table 必须整体落在同一个 file-backed、`U|R` LOAD 区间。

`PT_GNU_STACK` 不要求 X 时接受，用户栈始终 RW+NX；要求 executable stack 的 ELF 被拒绝。当前不支持共享同一 virtual page 且需要不同权限的重叠 LOAD；此类 ELF 由 page-table `AlreadyMapped` 失败并归类为 `ENOEXEC`。

### 3.3 链接脚本

`user/linker.ld` 显式声明 `PHDRS`：

- 第一个 RX LOAD 使用 `FILEHDR PHDRS`，file offset 为 0，ELF/program headers 与 entry 共同被映射；
- rodata 使用独立 R LOAD；
- data/BSS 在存在时使用独立 RW LOAD；
- `PT_GNU_STACK` 为 RW，不带 X；
- `__global_pointer$` 由链接脚本定义，`_start` 在 relaxation 禁用区间初始化 `gp`。

当前 init 的实际 ELF 为 `ET_EXEC`、entry `0x10158`、4 个 program headers；PHDR file range `0x40..0x120` 完整位于第一个 `offset=0, vaddr=0x10000, R|X` LOAD 内，因此 `AT_PHDR=0x10040` 是有效用户指针。

## 4. Linux/riscv64 初始栈

`sp` 始终按 16 bytes 对齐，栈内容按 RV64 word 排列：

| 顺序 | 内容 |
|---:|---|
| 1 | `argc` |
| 2 | `argv[0..argc]` |
| 3 | `NULL` |
| 4 | `envp[]` |
| 5 | `NULL` |
| 6 | `AT_PHDR, phdr_address` |
| 7 | `AT_PHENT, 56` |
| 8 | `AT_PHNUM, e_phnum` |
| 9 | `AT_PAGESZ, 4096` |
| 10 | `AT_ENTRY, e_entry` |
| 11 | `AT_NULL, 0` |
| 12 | argv/envp 原始字节串与终止 NUL |

kernel-created init 得到 `argc=1, argv[0]="/bin/init"`。`execve` 对 NULL 或空 argv 按 Linux 当前行为插入空 `argv[0]`，不向新映像暴露 `argc=0`。

边界为：

- pathname 包含 NUL 最多 4096 bytes；
- 单个 argv/envp string 包含 NUL 最多 32 pages（128 KiB）；
- argv/envp 字符串与指针计费共用 128 KiB 上限；超出返回 `E2BIG`；
- 初始栈总映射为 256 KiB，上下都不映射 guard page。

`AT_RANDOM`、`AT_HWCAP`、`AT_BASE`、TLS 和 vDSO 目前不提供；没有可证明 entropy/ISA capability/loader 模型前不填伪值。

## 5. `execve` 事务与 errno

`execve` 的顺序固定为：

1. 在旧 AddressSpace 锁下复制 path/argv/envp，所有字节成为 kernel-owned `Vec<u8>`。
2. 相对 path 按当前 cwd 解析；当前 cwd 唯一可能值为 `/`。

> 后续状态：Phase 22 已将 cwd 收敛为 Process-owned directory inode，并使 relative `execve` 直接从该 inode 解析；本条只记录 Phase 12 当时边界。
3. VFS 对 raw byte components 逐层查找，要求 regular inode，并在当前 root identity 模型下要求至少一个 execute mode bit。
4. loader 要求 full read；short read 是 I/O error，不在零填充 buffer 上解析 ELF。
5. 创建全新 MemorySet、LOAD/BSS、heap boundary、stack、auxv 与 trap-context page。
6. 只有上述全部成功后，才用一次 owner 替换提交 AddressSpace，然后写入新 TrapContext。
7. trap 返回路径识别共享 `SYSCALL_EXECVE` 常量；成功时不把 syscall 返回值覆盖到新映像的 `a0`。

| 失败 | errno |
|---|---:|
| path 缺失 | `ENOENT` |
| 中间分量非目录 | `ENOTDIR` |
| 当前不支持的 symlink resolution | `ELOOP` |
| directory/非 regular file/无 execute bit | `EACCES` |
| inode/block I/O 或 short read | `EIO` |
| 坏 ELF、PIE/dynamic/TLS/权限或 alignment 不支持 | `ENOEXEC` |
| path/argv/envp copy fault | `EFAULT` |
| path 过长 | `ENAMETOOLONG` |
| argv/envp 超限 | `E2BIG` |
| file buffer、copyin buffer、frame 或 page-table allocation 失败 | `ENOMEM` |

当前没有 fd table、signal disposition 或 sibling thread，因此 CLOEXEC、signal reset 和“exec 终止其他线程”不伪造占位状态。当这些子系统真正加入时，必须扩展同一提交事务。

## 6. 最小用户态

`user/src` 只保留：

- `lib.rs`：assembly `_start`、`gp` 初始化、initial-stack 解析、weak `main`；
- `syscall.rs`：Linux/riscv64 raw ecall，以及 `exit_group/write/sched_yield` 三个实际使用的 wrapper；
- `console.rs`：path-independent panic/output formatting；
- `lang_item.rs`：panic 输出后 `exit_group(127)`；
- `bin/init.rs`：输出一次 `LiteOS init` 后在 `sched_yield` 上保持 PID 1 存活。

删除了没有当前消费者的 657 行自定义 allocator、116 行 test utility 及其 alloc feature/export。镜像 allowlist 只写入 `/bin/init`，并显式将 inode mode 设为 `0100755`。

## 7. musl 边界

当前 initial stack 形式与 auxv 基础项可被 Linux/riscv64 crt/libc 消费，但 LiteOS 仍不是完整 musl runtime target。已知硬阻断为：

- 无 `mmap/munmap/mprotect`；
- 无 `openat/read/close/fstat`；
- 无 `clone/futex/set_tid_address/set_robust_list`；
- 无 signal ABI；
- 无 TLS 安装和 thread pointer 模型；
- 无 dynamic interpreter/relocation；
- 无经证明的 `AT_RANDOM`/entropy 与 `AT_HWCAP`/ISA capability 输出。

因此 README 在 Phase 13 只能声明“静态自带 RV64 ELF runtime”，不能声明“musl 兼容”。

## 8. 心智验收

1. 第一 LOAD 同时包含 file offset 0 的 ELF header、`0x40..0x120` PHDR 表和 entry；`AT_PHDR` 指向 `U|R` mapping。
2. 任意 LOAD 的 `p_filesz > p_memsz`、range overflow、file truncation、W+X、错误 alignment 或未映射 entry 都在提交前返回 `ENOEXEC`。
3. BSS 所在 frame 在复制 file prefix 前已整页清零，因此 `p_memsz - p_filesz` 不读取 file 尾部垃圾。
4. init 的 `sp & 15 == 0`，`argc=1`，`argv[0]` 指向 `/bin/init\0`，envp NULL 后紧跟六对 auxv word。
5. 非 UTF-8 path/argv/envp 只按 `/` 和 NUL 分割，不经 `String::from_utf8`。
6. exec 的 file read、ELF parse、frame allocation 或 stack write 失败时，旧 AddressSpace/TrapContext 保持不变。
7. exec 提交后无任何可返回错误的分配或解析步骤；新 entry 不被旧 `sepc+4` 覆盖。

## 9. 验证结果

- `git diff --check` 与 `cargo check --workspace` 通过；kernel 保持 9 个既有 warning，最小 user crate 无 warning。
- `make build-user`、`make build-kernel`、`make build-bootloader` 全部通过。
- `llvm-readelf -h -l` 确认 init 为 ELF64/LE/RISC-V/ET_EXEC，entry `0x10158`，PHDR offset `64`、entry size `56`、count `4`；第一 RX LOAD 从 file offset 0 开始并完整包含 PHDR。
- `llvm-objdump` 确认 `_start` 使用禁止 relaxation 的 `auipc/addi` 序列初始化 `gp`，将初始 `sp` 传入 `__user_start`。
- `python3 create_fs.py create` 成功生成 128 MiB、4 KiB block 的 ext2 镜像；debugfs 确认 `/bin/init` 为 regular inode、mode `100755`。
- 两轮 QEMU `virt -smp 8` 冷启动分别由 boot hart 1 和 6 开始；8 个 hart 全部上线，RTC、VirtIO block、ext2 mount、ELF load 和 init 入队成功，用户态两次均输出 `LiteOS init`，无 panic/fault。
- `cargo fmt --all -- --check` 仍只因本阶段未修改的 `kernel/src/arch/mod.rs` 与 `kernel/src/arch/riscv64/mod.rs` 导出排序失败；本阶段所有修改的 Rust 文件已单独通过 rustfmt。
- 静态检索确认不存在 UTF-8 exec copyin、`from_elf_with_args`、Option loader、user allocator/test utility、额外 user binary 或私有 syscall wrapper。
- 按仓库规则未执行、维护或修正测试用例。
