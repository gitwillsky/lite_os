# Phase 17：固定 musl 静态 consumer

## 固定输入

- musl `v1.2.6`，tag commit `9fa28ece75d8a2191de7c5bb53bed224c5947417`。
- 官方 release：`https://musl.libc.org/releases/musl-1.2.6.tar.gz`。
- tarball SHA-256：`d585fd3b613c66151fc3249e8ed44f77020cb5e6c1e635a616d3f9f82460512a`。
- consumer：`user/musl-smoke.c`，静态 RV64GC LP64D `ET_EXEC`，无 `PT_INTERP/PT_DYNAMIC/PT_TLS`。

源码与构建产物只存在于忽略的 `target/musl-static/`。仓库不 vendor musl，不使用 Zig 内置的其他 musl revision，也不维护第二套 libc patch。

## 真实调用链

`ELF LOAD -> initial argc/argv/envp/auxv -> musl crt1 -> __init_libc -> built-in main-thread TLS -> set_tid_address -> main -> exit_group`

consumer 在 `main` 中验证空 envp、`AT_PAGESZ` 经 `sysconf` 可见、PID、heap 分配释放、monotonic clock 和 stdout。成功输出 `LiteOS musl static ok`。`AT_RANDOM` 当前缺失；固定 musl 对空值有明确的 stack-canary fallback，因此它不是本 consumer 的启动阻塞，但也不提供任何熵或安全随机承诺。

## 围栏

`scripts/verify_musl.py` 执行单一路径：

1. 下载官方 tarball并校验 SHA-256；
2. 使用本机 RISC-V GCC 构建静态 musl；
3. 显式使用 musl crt/libc 与 GCC runtime 链接 smoke；
4. 静态拒绝非 RV64 `ET_EXEC`、dynamic/interpreter/TLS、W+X LOAD、可执行栈及 PHDR 不在 offset-zero LOAD 的产物；
5. 通过 `create_fs.py --init` 生成独立 ext2 镜像；
6. QEMU `-smp 1` 冷启动并要求 topology 与 `LiteOS musl static ok` marker。

Rust init 的 `-smp 1/3/8` 功能围栏和 musl consumer 围栏共享 `scripts/qemu_gate.py`，不存在两套 QEMU 进程控制或输出判定逻辑。

## 结论与边界

固定 musl 最小静态 consumer 已经真实启动，首次验收没有要求新增 syscall 或 kernel fallback。当前证明范围不包含 pthread、TLS program header、stdio/locale、文件遍历、signal interruption、process-directed signal、PIE、动态链接、共享/file mapping 与安全随机数。

最终执行 `make verify`：架构 fence、Rust workspace check/clippy、三组件构建、ELF 静态围栏、Rust init QEMU `-smp 1/3/8` 和固定 musl 静态 consumer QEMU 冷启动全部通过。

后续 Phase 18 将同一个 consumer/gate 前进到 `pthread_create/join`；本文件保留首次纯静态启动时的历史证据，不代表当前验收上限。
