# LiteOS Phase 8：VFS、文件对象与标准文件 syscall

> 审计日期：2026-07-11（Asia/Shanghai）
> 代码基线：提交 `f1d9581`（Phase 0–7）
> 验证约束：不维护、不修正、不执行测试；只做构建、静态 ABI/路径审计和非测试 QEMU 启动观察。

## 1. 阶段结论

Phase 8 不实现一组没有创建路径的文件 syscall，而是删除无法成立的 fd/OFD 假象，将当前能力收缩为：

- 内核 ELF 加载器通过唯一根 VFS 只读访问 ext2 inode；
- 用户态只保留 bootstrap `write(1)` 和 `getcwd(2)`；
- 不存在用户可见的普通文件 fd、open file description 或 fd table；
- 所有未形成标准语义闭环的文件 syscall 均落入统一 `-ENOSYS`。

这不是完整 Linux 文件 ABI。它的不变量是：未支持能力不会通过同名 syscall、忽略 flags 或不共享 offset 的对象冒充已实现。

## 2. 改动前的实际调用链

### 2.1 启动文件路径

`VirtIO block -> device_manager -> ext2/FAT32 fallback -> VFS mount map -> vfs.open -> inode.read_at -> ELF loader`

实际镜像由 `create_fs.py` 生成 ext2，FAT32 只是探测失败后的备用实现，没有对应构建路径。

### 2.2 用户 fd 路径

`sys_read/sys_write/sys_close/sys_lseek/sys_dup/sys_fcntl -> Process.file -> BTreeMap<fd, Arc<FileDescriptor>> -> inode`

但代码中没有 `openat`、`pipe2`、`socket` 或任何可以向该表插入普通对象的路径。因此 fd 3 以上的所有 handler 都不可达。

## 3. 已确认问题

| 严重度 | 问题 | 被破坏的契约 | 后果 |
|---|---|---|---|
| Blocker | `FileDescriptor` 同时存 offset、status flags 和 descriptor mode | fd entry 与 open file description 必须分层 | `dup/fork` 的 descriptor flags、status flags 和 offset 无法正确共享 |
| Critical | offset 使用 `load + I/O + fetch_add` | 共享 OFD 的 read/write 必须串行化“取 offset、I/O、更新 offset” | 并发 dup/fork 读写可使用同一偏移并覆盖进度 |
| Blocker | fd table 没有任何对象创建入口 | syscall 支持必须有可达状态 | `close/lseek/dup/fcntl` 只是表面功能 |
| Major | `fcntl` 对 stdin 和普通 fd 返回不同的 `ENOSYS/EINVAL` 子集 | Linux command errno 与 fd 有效性顺序 | 同名 syscall 暴露不可证明的部分语义 |
| Critical | `read(0)` 在无输入时反复 yield 并轮询 SBI | Phase 6 要求阻塞对象由等待队列和事件唤醒 | 无输入时任务始终 runnable，持续占用 CPU |
| Major | VFS 接受但忽略 open flags，同时保留无调用 mount/mutation API | flags 必须改变打开行为；公开 API 必须对应实际能力 | 调用者会误以为 create/truncate/append/multi-mount 存在 |
| Critical | ext2 `read_at` 把块映射错误用 `unwrap_or(0)` 当作 sparse hole | I/O/元数据错误不能伪造零数据 | 损坏的 ELF 可被静默补零后交给加载器 |
| Major | loader 接受 short read 并返回含零尾的 buffer | 程序镜像必须完整读取 | 截断文件被伪装成完整 ELF |

## 4. 终态边界

### 4.1 VFS 与 inode

- `VirtualFileSystem` 只拥有一个根文件系统；重复挂载返回 `AlreadyExists`，不静默替换。
- `open` 是内核 ELF loader API，只接受绝对路径，不是 `openat(2)` handler。
- `.` 按当前 inode 处理；`..` 在已验证 inode 栈上回退，`/missing/..` 仍返回 NotFound。
- 路径不能逃出根；尾随 `/` 要求最终 inode 是目录。
- 不跟随 symlink；遇到 symlink 明确拒绝，不伪装 Linux symlink resolution。
- inode 公共接口只保留 type、size、read-at 和 direct-child lookup。

### 4.2 文件系统

- 只保留 ext2；删除 FAT32 实现和 probe fallback。
- ext2 只读；删除 write/create/remove/truncate/xattr/bitmap allocation/transaction/cache flush 全链。
- 文件系统块与设备块的换算由唯一 read helper 完成，GDT 不再把文件系统块号直接当设备块号。
- 根 inode 无效、目录项边界无效、间接块 I/O 失败均返回错误；不 `unwrap`、不当作 hole。
- 本阶段 ELF loader 只接受 regular file，并曾采用整文件读取；当前权威架构已由 inode-backed `ExecutableSource` 与 PT_LOAD 逐页读取取代该阶段实现。

### 4.3 fd 与 syscall

- 当前不定义 `FileDescriptorTable`、fd entry 或 `OpenFileDescription`；不存在错误分层。
- `write(1)` 是启动输出的显式例外，直接写 SBI DBCN；其他 fd 返回 `EBADF`。它不宣称是通用 fd/OFD 实现。
- `getcwd(2)` 返回含 NUL 的长度；缓冲区过小返回 `ERANGE`，copyout 失败返回 `EFAULT`。cwd 当前固定为 `/`，因为不暴露 `chdir`。
- `read`、`close`、`dup`、`fcntl`、`lseek` 从共享 ABI 和 dispatcher 删除，包括无等待队列的 stdin 轮询。

## 5. ABI 支持矩阵

| 编号 | Linux/riscv64 名称 | 状态 | 当前契约 |
|---:|---|---|---|
| 17 | `getcwd` | Complete | 当前唯一 cwd `/`；长度、NUL、ERANGE/EFAULT 语义明确 |
| 64 | `write` | Partial | 仅 fd 1 -> SBI DBCN；支持 partial write/EFAULT/EIO；无通用文件对象 |
| 63 | `read` | Removed | 未知 syscall 路径返回 `ENOSYS`；无事件唤醒前不恢复 |
| 23/24 | `dup`/`dup3` | Removed/Missing | 无 OFD 不暴露 |
| 25 | `fcntl` | Removed | 无 fd/OFD flags 分层不暴露 |
| 56/57 | `openat`/`close` | Missing/Removed | 无用户文件打开闭环 |
| 59 | `pipe2` | Missing | Phase 9 审计决定 |
| 61/62 | `getdents64`/`lseek` | Missing/Removed | 无用户 fd/OFD |
| 29/35/46/48/53/79/80 | `ioctl`/`unlinkat`/`ftruncate`/`faccessat`/`fchmodat`/`newfstatat`/`fstat` | Missing | 不提供近似实现 |

## 6. 删除内容

- syscall number/dispatch/handler：`read`、`close`、`lseek`、`dup`、`fcntl`。
- 进程模型：`FileDescriptor`、`File`、fd BTreeMap、offset atomics、CLOEXEC 遍历和 exec 关闭步骤。
- VFS：multi-mount map、unmount、ignored open flags、relative cwd resolution、create/remove/get-inode 转发层。

> 后续状态：Phase 22 已以 Process-owned cwd inode 实现标准 `chdir(49)`、relative lookup 与 VFS reverse `getcwd`；本文关于 cwd 固定为 `/` 的内容只记录 Phase 8 当时边界。
- 文件系统：FAT32 整个模块；ext2 写入、分配、回收、xattr、transaction、metadata/bitmap cache 和统计平面。
- 输入：SBI DBCN getchar buffer/helper 和 runnable polling loop。
- 因上述删除成为孤儿的 user-range preflight wrapper 与秒级 Unix timestamp wrapper。

## 7. 心智验收

1. 路径 `/bin/./init` 逐层读取成功；`/bin/../bin/init` 在 inode 栈上回退后成功。
2. `/missing/..` 先查找 `missing` 并返回 NotFound，不被词法化简成 `/`。
3. `/../../bin/init` 的 `..` 停在根；不越界。
4. `/bin/init/` 最终 inode 非目录，返回 NotDirectory。
5. 任一中间 symlink 或最终 symlink 返回 InvalidOperation，不跟随。
6. 块映射 I/O 错误向 loader 传播；short read 不产生补零 ELF。
7. `write(fd != 1, ..., 0)` 先校验 fd 并返回 EBADF；`write(1, ..., 0)` 返回 0。
8. 用户调用已删除的标准编号时，dispatcher 统一返回 ENOSYS，不落入旧 handler。

## 8. 验证结果

- `cargo check --workspace`：通过；kernel warning 从 Phase 7 的 291 降至 258，Phase 8 相关文件无新 warning。
- `cargo fmt --all -- --check`：仓库基线仍因本阶段未修改的 `arch/mod.rs`、`arch/riscv64/mod.rs` 和 `drivers/block.rs` 排序/折行差异失败；未为了格式检查扩大 diff。
- `make build-user`、`make build-kernel`、`make build-bootloader`：通过。
- `python3 create_fs.py create`：成功生成 128 MiB、4 KiB block 的 ext2 镜像并写入 `/bin/init`。
- 两轮 8-hart QEMU 冷启动：全部 hart 上线，ext2 根挂载成功，init 创建并入队；观察窗口内无 panic/fault。
- 静态检索：生产代码不存在 FAT32、fd table/FileDescriptor、`read/close/lseek/dup/fcntl` handler 或 SBI getchar 残留。
- 按仓库规则未执行、维护或修正测试用例。

## 9. 尚未证明的风险

- ext2 只读路径没有经过故意损坏镜像的动态验证；当前只能证明边界检查与错误传播链存在。
- ext2 仍以 `i_size_lo` 为文件大小，不宣称支持 large-file 高 32 位。当前启动镜像的 init 不依赖该能力。
- `write(1)` 是 bootstrap console 特例，不能支撑 musl 的通用 fd 假设；Phase 11/12 必须在支持矩阵和最小 userspace 中继续显式标记。

## 10. Phase 9 计划

1. 扫描 pipe、shared memory、Unix socket 与 poll 残留 → verify：每个 IPC 机制都能追溯到标准 syscall 和唯一等待队列。
2. 删除没有标准 fd 表达的 IPC 实现 → verify：不留私有 handle、轮询或每机制独立唤醒模型。
3. 判定 pipe2 是形成标准 fd/OFD 的第一个竖切还是继续不支持 → verify：只在 close/dup、partial I/O、blocking/wakeup 全部可证明时接入 ABI。
4. workspace 构建与两轮 8-hart QEMU 启动 → verify：无新 warning、panic、fault 或调度状态不变量失败。
