# LiteOS userspace

当前产品为 BusyBox init、网络服务、Wayland compositor、可信 logon、用户 desktop 与文件浏览器；UART 保留恢复 shell。
首次账户设置与验证见[桌面验证](../docs/development/desktop-verification.md)。用户态媒体程序仍待后续实现。

| Path | Interface | Owner |
|---|---|---|
| `base/` | BusyBox 配置、身份与 init 策略 | rootfs builder |
| `linux-uapi/` | 安全 typed Linux interface | DRM/evdev/ALSA/PTY/memfd/process/poll/SCM_RIGHTS raw ABI |
| `keyboard/` | 键盘布局与编码 | keysym、PTY 输入编码 |
| `terminal-model/` | VT model | parser、screen、scrollback、selection |
| `terminal-font/` | terminal atlas | 字形索引与网格尺寸 |
| `canvas/` | CPU 2D 光栅 | 填充、斜边、渐变、抖动、XOR、alpha blit、蒙版着色（物理像素，无 unsafe） |
| `classic-theme/` | 经典主题 | 系统颜色、窗口度量、立体边框取色、Wine 注册表；数据在 `assets/classic/theme/classic.theme` |
| `wayland-scanner/` | 协议生成器 | Wayland 协议 XML → Rust 类型（`wayland-wire` 的 build 依赖） |
| `wayland-wire/` | Wayland wire | 对象 ID、消息帧、参数读写与上游协议 XML 生成的接口类型；不拥有 socket |
| `classic-ui/` | 经典控件绘制 | `Painter`：边框、按钮、复选/单选、编辑框、滚动条、菜单、窗口框架与标题栏、提示；逻辑像素 × 整数 scale |
| `typeface/` | 字体与文本 | BDF 点阵、sfnt 轮廓与内嵌点阵、灰度光栅、中西文选择、仿粗体、CJK 换行 |
| `compositor/` | 显示服务 | 唯一 DRM master 与 evdev 读者；Wayland 服务端（`wl_compositor`/`wl_shm`/`xdg_wm_base`/`wl_output`）、窗口栈与整帧 CPU 合成；`wayland-probe` 是门禁客户端 |
| `logon/` | 本机登录与会话监督 | shadow 认证、锁定/改密、凭据、桌面进程组与租约 |
| `desktop/` | 用户桌面外壳 | 图标、任务栏、开始菜单、运行和应用任务窗口 |
| `file-explorer/` | 普通 Wayland 应用 | 目录导航与有界 UTF-8 文本预览 |
| `diagnostics/` | cputest、memtest、cachetest | 有界 multicall diagnostics |

音频混音归属 kernel ALSA；用户态无需音频服务或私有音频协议。接口与失败路径见
[用户态契约](../docs/architecture-contract/userspace-abi.md)和[音频契约](../docs/architecture-contract/audio.md)。
