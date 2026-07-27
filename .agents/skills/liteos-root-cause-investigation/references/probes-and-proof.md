# 探针与证明

## 可信复现

### 固化身份

每份日志开头保存：

```bash
git status --short
git rev-parse HEAD
rg -n '^(ARCH|ACCEL|PROFILE|FS_IMAGE_SIZE_MIB|QEMU_MEMORY|QEMU_SMP) \\?=' Makefile
LITEOS_ARCH=aarch64
file "target/rootfs/${LITEOS_ARCH}.img" "fs-${LITEOS_ARCH}.img"
```

同时记录实际命令行；Make 变量、artifact 和 QEMU 参数三者必须一致。

### 隔离镜像

`scripts/qemu_gate.py::boot()` 默认复制 private image，优先复用它。手工实验需要持久写入时：

1. 用 `mktemp -d` 创建本次实验的唯一目录；
2. 从已记录摘要的源镜像复制；
3. 只让一台 QEMU 使用该副本；
4. 实验后把需要保留的日志、镜像摘要和复现命令移入诊断目录，其余产物可恢复地清理。

修改开发实例前，设置 `LITEOS_ARCH` 并执行 `lsof -- "fs-${LITEOS_ARCH}.img"`，确认无 QEMU
持有。镜像锁失败属于 host orchestration 证据，不属于 guest kernel 证据。

### 防止 marker 假阳性

交互终端会回显输入。期望 marker 的完整字节序列不能出现在注入命令中，例如：

```sh
n=$((6 * 7)); printf 'LITEOS_PROBE_%s\n' "$n"
```

host 等待 `LITEOS_PROBE_42`。同时设置 forbidden marker，并在成功 marker 后保留足够 settle
窗口观察迟到 panic。只有 gate 确认全部 interaction 已注入、目标 phase 已执行，marker 才有效。

### 冷热状态

同一 QEMU 进程内的 reset 不能替代 cold boot。持久化实验固定为：

```text
启动 private image → 执行 workload → 明确 sync/退出边界 → 结束 QEMU
→ 用同一 private image 启动新 QEMU 进程 → 验证
```

## 窄探针

优先级从低侵入到高侵入：

1. 现有 errno、panic、phase marker 和 artifact metadata；
2. wrapper/native、SMP、cold/warm 等单变量对照；
3. 已有 unit/model test 中的 production state transition；
4. 最小 musl shell/C fixture；
5. 临时 caller/context/owner generation 日志；
6. GDB 或 target `addr2line`。

内核地址使用当前同 profile 的 ELF：

```bash
LITEOS_PC=0xffff000040012345 # 替换为本次串口记录的 PC
make PROFILE=release addr2line ADDR="$LITEOS_PC"
```

临时日志必须有唯一前缀，修复后用 `rg` 证明前缀、诊断 flag 和 caller trace 已全部删除。

## 回归选择

把回归放在能执行 production path 的最低层：

| first broken contract | 首选回归 |
|---|---|
| 纯状态转换/数据结构 | `kernel-unit` 或对应 host model |
| syscall ABI/errno | `syscall-abi` 加最小 musl fixture |
| scheduler/wait membership | scheduler/kernel unit 加 runtime lifecycle |
| trap、MMU、IPI、设备 | release/static gate 加目标 QEMU runtime |
| rootfs、ELF、init、shell | BusyBox/musl/Rust std runtime gate |
| GUI/音频/输入 | 对应 production UI/audio/frame gate |

测试必须在旧实现上命中原 first broken contract；复制一份 production algorithm 得到自洽结果不构成
回归。

## 证明梯度

先重跑完全相同的红色命令，再按风险扩展：

```bash
make verify-unit
make verify-architecture-benchmark
make verify-architecture-release
make verify-runtime-busybox
make verify
make verify-riscv64-secondary
git diff --check
```

`verify-runtime-busybox` 是命令形态示例；根据 first broken contract 从 Makefile 选择对应
`verify-runtime-*` owner。

选择规则：

- 修改通用 kernel façade、syscall 或 owner：执行 AArch64 相关 runtime，并跑 RISC-V secondary；
- 修改 arch/platform：执行该 target 的 release/static/boot gate，通用 seam 变化再跑 secondary；
- 修改 rootfs/userland：执行拥有该产物的 runtime gate及真实 guest 命令；
- 修改并发、retirement、cleanup：覆盖触发竞争的 SMP topology 与 cold restart；
- 新增热路径、锁、分配、codec 或间接层：按 `docs/development/build-and-verify.md` 作 benchmark
  决策。

`make verify` 提前失败时，保存它到达的最后 phase。随后只补跑被截断且与本次改动相关的正式目标；
最终报告分别列出：

```text
已通过：
目标失败：
因提前拦截未执行：
独立补跑：
与本次改动无关的 blocker：
```

完整门禁只在 `make verify` 自身退出成功时标记为通过。
