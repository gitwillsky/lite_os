# LiteOS userspace

LiteOS userspace 的交互产品轨道是 XP 风格图形桌面：`compositor` 独占 DRM/evdev、scanout、合成与
输入路由，React `desktop` 独占窗口 policy 与产品呈现。所有窗体应用共用 `/bin/lite-ui`；无窗体
程序与 3D 游戏不经过 LiteUI。TUI 程序仍只看到标准 PTY/termios/ANSI。

## Modules

| Path | Interface | Owner |
|---|---|---|
| `base/` | BusyBox configuration, identities and init policy | rootfs builder |
| `display-proto/` | graphical session wire 与 SCM_RIGHTS transport | scene/surface/buffer 协议语义 |
| `compositor/` | `/bin/compositor` | DRM master、evdev、scanout、合成、输入与 session epoch |
| `quickjs-runtime/` | LiteUI 内部安全接口 | vendored QuickJS C ABI、VM lifetime 与执行边界 |
| `lite-ui/` | `/bin/lite-ui` | QuickJS/React host、CSS/layout/text/raster 与 app lifecycle |
| `terminal-session/` | `/bin/terminal-session -- <argv>` | PTY、VT screen、scrollback 与 selection |
| `linux-uapi/` | safe typed Linux-specific interface | DRM/evdev/PTY/process/poll/SCM_RIGHTS raw ABI |
| `diagnostics/` | `cputest`, `memtest`, `cachetest` multicall executable | bounded product diagnostics |

`compositor` 启动后立即显示 native boot scene，直到 React desktop 首个完整 scene latch；不再存在
独立 splash process。共享 DRM OFD 只是当前可信 GUI 进程间的 mapping mechanism：buffer 只能由
compositor 创建和销毁。UART shell 仍是 BusyBox init 拥有的独立恢复路径。完整 owner/interface 与
failure 契约见 [`docs/architecture-contract/lite-ui.md`](../docs/architecture-contract/lite-ui.md)。
