# LiteOS userspace

LiteOS userspace 的交互产品轨道是 XP 风格图形桌面：`desktop` 独占 DRM/evdev 并合成所有窗口，
`terminal` 作为桌面客户端承载标准 Linux TUI 程序。应用程序仍是普通 musl 可执行文件或 Alpine
APK，看到的仍是 PTY/termios/ANSI；桌面客户端协议属于 `display-proto` 定义的内部 seam。

## Modules

| Path | Interface | Owner |
|---|---|---|
| `base/` | BusyBox configuration, identities and init policy | rootfs builder |
| `display-proto/` | 桌面客户端协议 wire 定义与 SCM_RIGHTS 传输 | 协议消息、buffer 所有权规则 |
| `desktop/` | `/bin/desktop`；init respawn 的唯一图形入口 | DRM master、evdev、合成、窗口管理与 shell UI |
| `terminal/` | `/bin/terminal`；桌面客户端，应用只见 PTY/termios/ANSI | ANSI parser、终端 renderer、PTY 监督 |
| `splash/` | `/bin/splash`；init sysinit 的启动画面 | 临时屏幕 owner，桌面首帧后被 SIGTERM 接管 |
| `diagnostics/` | `cputest`, `memtest`, `cachetest` multicall executable | bounded product diagnostics |

`desktop` 是 DRM master 与输入的唯一 owner：`splash` 在 sysinit 首个 open `/dev/dri/card0`
绘制启动画面后 DROP_MASTER，`desktop` 经 SET_MASTER 取得 master，握手时经 SCM_RIGHTS 把同一
OFD 共享给客户端；客户端在其上 CREATE_DUMB 并把 handle 交给桌面合成，handle 的 DESTROY 只由
桌面执行。`terminal` 是 terminal model 的唯一 owner。UART shell 仍是 BusyBox init 拥有的独立
恢复路径。ABI verification consumers live under `scripts/fixtures/` and never enter the product
source tree. Standard Rust applications use the official Linux/musl target and the repository-owned
musl runtime; the `std` ABI proof remains a verification fixture rather than a second product
userspace track.
