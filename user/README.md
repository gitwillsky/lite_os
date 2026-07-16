# LiteOS userspace

`user/` 只保存进入产品 rootfs 或直接证明产品 ABI 的源码。构建产物、下载缓存、生成的 APK、
字体与 terminfo binary 分别属于 `target/`、`assets/` 或构建脚本，不得复制回本目录。

当前实现事实见 [`docs/architecture.md`](../docs/architecture.md)，稳定 owner/interface 见
[`docs/architecture-contract.md`](../docs/architecture-contract.md)。Display、terminal 与桌面的详细
seam 分别见
[`display-terminal.md`](../docs/architecture-contract/display-terminal.md) 和
[`desktop.md`](../docs/architecture-contract/desktop.md)。本文只负责源码导航，不复制能力清单。

## Layout

| 路径 | 类型 | 唯一职责 |
|---|---|---|
| `base/` | rootfs policy | BusyBox config、identity、init、network、shutdown 与 terminfo source |
| `probes/` | ABI consumer | musl、dynamic loader/shared-object 的固定验证程序，不拥有产品 runtime |
| `diagnostics/` | product diagnostic | 单一 `liteos-stress` ELF 的源码；rootfs 只以 `cputest/memtest/cachetest` hardlink 暴露 |
| `apps/` | LiteUI application source | System Shell、Calculator 与共享 Solid HostOps adapter；APK 是派生产物 |
| `service-activation/` | Rust rlib | activated listener 的唯一解析 seam |
| `display-client/` | Rust rlib | pinned libseat client lifecycle adapter |
| `display-session/` | Rust executable | DRM/input capability broker 与同步 revoke owner |
| `liteui-core/` | Rust rlib | 无 I/O retained UI、transaction、DrawList 与 TextGrid |
| `liteui-compositor/` | Rust executable | 唯一 DRM/evdev/window/render owner |
| `liteui-host/` | Rust executable | 一个进程、一个 APK、一个 QuickJS Runtime |
| `liteui-session/` | Rust executable | 图形 generation、identity、spawn/reap/restart owner |
| `terminal-service/` | Rust executable | PTY、ANSI model、input encoder 与 TextGrid publication owner |

Rust module 直接位于 `user/` 顶层，因为其目录名就是架构 module identity。再包一层 `crates/` 只会
增加路径长度，不会形成新的深 module 或 seam。`base/probes/diagnostics` 则按生命周期和 consumer
分组，避免把 rootfs policy、验证程序与产品进程混在同一层。

## Build contract

- 每个 Rust module 都是独立标准 crate；root workspace 显式 exclude，唯一 rootfs builder 负责
  Linux-musl `core/alloc/compiler_builtins` 与最终 CRT/libc 链接。禁止恢复第二个 userspace workspace、
  linker script、私有 syscall runtime 或平行 init。
- `Cargo.lock` 属于独立 `--locked` 构建输入，即使当前 dependency-free 也不是冗余文件。
- application 的 `app.mjs`/`styles.css` 与 `apps/runtime/app-runtime.mjs` 是权威 source；
  `manifest.cbor`、`styles.bin`、bundle 与 APK 必须由 `scripts/liteui_package.py` 派生。
- 跨 module 复用只有在形成真实 interface 且至少存在两个 adapter 时才建立 seam。相似的 FFI 声明、
  allocator failure policy 或 process-specific diagnostics 不应为消除文本重复而抽成浅 module。

## Change checklist

1. 先确定修改所属 module、状态 owner、调用 interface 与 exit/error cleanup。
2. 新增顶层条目时，同步本索引和 architecture checker 的精确集合；未登记文件应被拒绝。
3. 删除源码前必须证明全部 build、rootfs、runtime 与文档 consumer 已消失；禁止用“看起来没用”代替引用证据。
4. 所有 production Rust/C source 保持 600 行硬上限，超限沿 owner/interface seam 拆分，不登记例外。
