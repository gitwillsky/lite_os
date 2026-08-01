# LiteOS userspace

LiteOS userspace 的交互产品轨道是 Aurora React 图形桌面：`compositor` 独占 DRM/evdev、scanout、合成与
输入路由，React `desktop` 独占窗口 policy 与产品呈现。`lite-runtime` 是通用 GUI/JS 运行时**库**，
每个窗体应用是链接它的独立二进制 `/bin/<id>`（`desktop`/`file-manager`/`my-computer`/`terminal`/
`music-player`）；应用专属 native 能力经 `HostExtension` 注入（如 music-player 的 provider/TLS/流式
下载）。无窗体程序与 3D 游戏不经过 LiteUI。TUI 程序仍只看到标准 PTY/termios/ANSI。

## Modules

| Path | Interface | Owner |
|---|---|---|
| `base/` | BusyBox configuration, identities and init policy | rootfs builder |
| `display-proto/` | graphical session wire 与 SCM_RIGHTS transport | scene/surface/buffer 协议语义 |
| `compositor/` | `/bin/compositor` | DRM master、evdev、scanout、合成、输入与 session epoch |
| `quickjs-runtime/` | LiteUI 内部安全接口 | vendored QuickJS C ABI、VM lifetime 与执行边界 |
| `lite-runtime/` | `lib lite_runtime`（被各 app bin 链接） | QuickJS/React host、CSS/layout/text/raster、fs.* 与音频播放；扩展缝 `HostExtension` |
| `apps/` | `/bin/{desktop,file-manager,my-computer,terminal,music-player}` | 各应用二进制（链接 lite-runtime）；desktop=启动器策略，music-player=provider/TLS/流式 |
| `terminal-session/` | `/bin/terminal-session -- <argv>` | PTY、VT screen、scrollback 与 selection |
| `audio-proto/` | AF_UNIX v1 与 memfd SPSC ring | stream identity、frame codec 与共享 PCM publication |
| `audio-service/` | `/bin/audio-service` | ALSA device clock、stream quota、mixer、limiter 与 master state |
| `linux-uapi/` | safe typed Linux-specific interface | DRM/evdev/ALSA/memfd/process/poll/SCM_RIGHTS raw ABI |
| `diagnostics/` | `cputest`, `memtest`, `cachetest` multicall executable | bounded product diagnostics |

`compositor` 启动后立即显示 native boot scene，直到 React desktop 首个完整 scene latch；不再存在
独立 splash process。共享 DRM OFD 只是当前可信 GUI 进程间的 mapping mechanism：buffer 只能由
compositor 创建和销毁。UART shell 仍是 BusyBox init 拥有的独立恢复路径。完整 owner/interface 与
failure 契约见 [`docs/architecture-contract/lite-runtime.md`](../docs/architecture-contract/lite-runtime.md)。
