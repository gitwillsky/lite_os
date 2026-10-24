# LiteOS userspace

当前产品基线为 BusyBox init、网络服务与 UART shell。显示器保留 kernel `LBS1` 开机画面，
尚无登录、桌面或用户态媒体程序。新图形栈按[经典桌面计划](../docs/plans/classic-desktop.md)从公开协议实现。

| Path | Interface | Owner |
|---|---|---|
| `base/` | BusyBox 配置、身份与 init 策略 | rootfs builder |
| `linux-uapi/` | 安全 typed Linux interface | DRM/evdev/ALSA/PTY/memfd/process/poll/SCM_RIGHTS raw ABI |
| `keyboard/` | 键盘布局与编码 | keysym、PTY 输入编码 |
| `terminal-model/` | VT model | parser、screen、scrollback、selection |
| `terminal-font/` | terminal atlas | 字形索引与网格尺寸 |
| `diagnostics/` | cputest、memtest、cachetest | 有界 multicall diagnostics |

音频混音归属 kernel ALSA；用户态无需音频服务或私有音频协议。接口与失败路径见
[用户态契约](../docs/architecture-contract/userspace-abi.md)和[音频契约](../docs/architecture-contract/audio.md)。
