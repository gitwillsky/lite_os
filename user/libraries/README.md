# user/libraries — 库

这里每个目录都是一个 **库**（有 `src/lib.rs`），不会单独运行。目标系统上它们被构建成共享库（对应 Windows 2000 的 DLL），
安装到 `/usr/lib/liteos/`，由 `/etc/ld-musl-<arch>.path` 加入动态链接器搜索路径。

规则：
- 每个共享库的 `crate-type = ["dylib", "rlib"]`，并 `extern crate liteos_runtime as _;`，这样标准库只在 `liteos-runtime` 里有一份；
  rlib 用于探针与 host 测试的静态构建。
- Rust 没有稳定 ABI：库与链接它们的程序必须同一工具链、同一组参数、同一个 Cargo target 一次构建并一起安装。
- 层次：`liteos-runtime`（std）→ `canvas`/`typeface`/`classic-theme` → `classic-ui` → 程序；`linux-uapi` 是唯一的原始 FFI 出口。

| 目录 | 作用 | 产物 |
|---|---|---|
| [`liteos-runtime/`](liteos-runtime/README.md) | 运行时共享库 | `/usr/lib/liteos/libliteos_runtime.so` |
| [`canvas/`](canvas/README.md) | CPU 2D 光栅库 | 共享库 `libcanvas.so` |
| [`classic-theme/`](classic-theme/README.md) | 经典主题数据 | 共享库 `libclassic_theme.so` |
| [`typeface/`](typeface/README.md) | 字体与文本排版 | 共享库 `libtypeface.so` |
| [`classic-ui/`](classic-ui/README.md) | 经典控件与窗口 | 共享库 `libclassic_ui.so` |
| [`wayland-scanner/`](wayland-scanner/README.md) | Wayland 协议生成器 | 构建期工具库（不安装） |
| [`wayland-wire/`](wayland-wire/README.md) | Wayland 线协议 | 共享库 `libwayland_wire.so` |
| [`linux-uapi/`](linux-uapi/README.md) | Linux 内核接口封装 | 共享库 `liblinux_uapi.so` |
| [`keyboard/`](keyboard/README.md) | 键盘布局与编码 | 共享库 `libkeyboard.so` |
| [`calendar/`](calendar/README.md) | 公历日期换算 | 共享库 `libcalendar.so` |
| [`desktop-entry/`](desktop-entry/README.md) | 桌面条目解析 | 共享库 `libdesktop_entry.so` |
| [`desktop-settings/`](desktop-settings/README.md) | 每用户桌面设置 | 共享库 `libdesktop_settings.so` |
| [`sound/`](sound/README.md) | 系统提示音 | 共享库 `libsound.so` |
| [`sndvol/`](sndvol/README.md) | 音量状态与布局 | 共享库 `libsndvol.so` |
| [`terminal-font/`](terminal-font/README.md) | 终端字体图集 | 静态库（目前没有程序使用） |
| [`terminal-model/`](terminal-model/README.md) | 终端模型 | 静态库（目前没有程序使用） |

新增共享库时：在这里建目录与 README，加 `liteos-runtime` 依赖与 `extern crate`，登记到 `user/Cargo.toml`、根 `Cargo.toml`、
`scripts/verify_busybox.py` 的 `SYSTEM_LIBRARIES` 与 `architecture-check` 的 `LIBRARIES`。
