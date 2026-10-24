# 计划：简体中文 Windows 2000 经典桌面与 Win32 运行时

Status: Active

> 本文件只在任务执行期间存在。每个阶段完成后，把持久事实迁入对应的 architecture / contract /
> syscall-support / design 文档；全部完成后删除本计划。

## 目标与验收

1. 从 kernel 第一帧到桌面，开机、登录、桌面的外观与交互 100% 还原简体中文 Windows 2000
   Professional（仓库保留原创素材；用户自备原版素材仅放在 Git 忽略的本地目录）。
2. 支持 HiDPI：在 3008×1692 等高分辨率输出上按整数 scale 渲染，几何、文字、图标、光标都清晰。
3. 用户态 GUI 全部自研；对外只暴露公开标准（Wayland、ALSA、evdev、freedesktop Desktop Entry），
   删除 LiteUI 与所有私有用户态协议。
4. 通过上游 Wine（WoW64）+ FEX 运行 i386 Win32 程序；先用标准 Win32 程序验证 API 与窗口，
   用户提供游戏文件后运行红色警戒 2 并能进入遭遇战且有声音。

## 已确认的决策

| 决策 | 结论 |
|---|---|
| Win32 兼容层 | 上游 Wine + FEX，作为 `/opt/wine` 第三方运行时；不自研 PE/x86/Win32 |
| 音频 | kernel 内多 substream 混音，删除 `audio-service` / `audio-proto` |
| 文本 VT | 删除 `console`、`seatd`、`seat-proto`；恢复路径只保留 UART shell |
| 界面语言 | 简体中文 |
| 登录 | 需要：Ctrl+Alt+Del 开始登录 → "登录到 LiteOS" → 桌面；会话内 Ctrl+Alt+Del 打开"LiteOS 安全" |
| 游戏文件 | 用户之后自行放入；本仓库不包含 |
| 架构范围 | 桌面支持 AArch64 与 RISC-V64；Wine/FEX 只支持 AArch64（FEX 无 riscv64 host），限制写入当前架构限制 |

## 组件与命名

### kernel（在现有 module 内修改）

| 位置 | 职责 |
|---|---|
| `drm/boot_screen.rs` | probe 后直接 scanout 第一帧：黑底 + 底部进度条；`main.rs` 在启动里程碑推进进度；首个 DRM master `SETCRTC` 时交出 scanout |
| `audio/engine.rs` | 一张卡暴露 N 个 playback substream（同 emu10k1 这类硬件混音卡），每次 open 分配一个 |
| `audio/mixer.rs` | 48 kHz float 混音与 limiter，写入 VirtIO Sound |
| `audio/control.rs` | `/dev/snd/controlC0`：`Master Playback Volume/Switch` 与每个 substream 的 `PCM Playback Volume` |
| 各 syscall / sysfs / DRM / evdev 领域 | 补齐 Wayland 与 Wine/FEX 所需的 ABI，逐项写入 ABI 矩阵 |
| `drm/virtgpu/`（后续） | Linux `DRM_IOCTL_VIRTGPU_*` 多 context 与 dmabuf，作为 D3D 的前提 |

### 用户态库

| crate | 职责 | 依赖 |
|---|---|---|
| `wayland-wire` | 纯 Rust Wayland wire 编解码；build 时由 vendored 协议 XML 经自研 `wayland-scanner` 生成服务端与客户端共用的类型；不拥有 socket（收发和 SCM_RIGHTS 归使用方，经 `linux-uapi`） | 无（build：`wayland-scanner`） |
| `classic-theme` | Win2000 视觉事实的唯一 owner：系统颜色、尺寸、字体名、动画时长；同时生成 Wine 前缀的 `Control Panel\Colors` / `Metrics` 注册表，保证 Win32 程序与桌面一致 | 无 |
| `canvas` | CPU 2D 光栅：填充、立体斜边、渐变、blit、alpha 混合、裁剪 | 无 |
| `typeface` | 字体加载、字形光栅与缓存、文本测量、CJK 换行 | `swash` |
| `classic-ui` | 经典控件库：Wayland 客户端窗口、客户端绘制的窗口边框、按钮、编辑框、菜单、ListView、TreeView、滚动条、属性页、对话框、焦点与键盘导航 | `wayland-wire`、`classic-theme`、`canvas`、`typeface`、`keyboard` |
| `keyboard`（保留） | 键盘布局与 keysym；新增导出 XKB keymap 文本 | 无 |
| `linux-uapi`、`terminal-model`、`terminal-font`（保留） | 不变 | — |

### 用户态程序

| 二进制 | 对应 Win2000 | 职责 |
|---|---|---|
| `/bin/compositor` | 显示子系统 + 窗口管理 | 唯一的 DRM master 与 evdev owner；VirGL 合成；Wayland 服务端；窗口叠放、焦点、移动缩放、最小化最大化；Alt+Tab；启动画面；Ctrl+Alt+Del 作为安全注意序列只投递给 `logon`；SPICE 剪贴板 |
| `/bin/logon` | winlogon + GINA | "按 Ctrl+Alt+Del 开始"、"登录到 LiteOS"（校验 `/etc/shadow`）、"LiteOS 安全"、锁定计算机、注销、关机；认证后以目标用户身份拉起 `desktop` |
| `/bin/desktop` | explorer 外壳 | 桌面图标层、任务栏、快速启动、开始菜单、托盘（音量、时钟）、"运行"、"关闭 LiteOS" |
| `/bin/file-explorer` | 资源管理器 + 我的电脑 | 一个程序，"我的电脑"是其中一个位置 |
| `/bin/command-prompt` | 命令提示符 | 自持 PTY，复用 `terminal-model` / `terminal-font`；替代 `terminal` + `terminal-session` |
| `/bin/task-manager` | 任务管理器 | 应用程序 / 进程 / 性能三页（读 `/proc`） |
| `/bin/media-player` | 媒体播放器 | Symphonia 本地解码直接写 ALSA；在线 provider 迁移现有 `MusicExt` 逻辑 |
| `/bin/control-panel`（后续） | 控制面板 | 显示属性、声音、日期时间 |
| `/opt/wine` | 第三方运行时 | Wine + FEX；winewayland 显示；cnc-ddraw GDI 渲染；winealsa 声音 |

### compositor 内部 module

```
compositor/src/
  main.rs                 装配
  boot.rs                 启动画面（改画 Win2000 风格并接续 kernel 进度）
  scanout.rs gpu/         沿用 VirGL 合成与 page flip
  cursor.rs               沿用硬件光标
  spice_agent.rs          沿用；剪贴板对接 wl_data_device
  input.rs                evdev → 指针/键盘语义事件
  wayland/                每种协议一个文件
    client.rs registry.rs surface.rs shm.rs subsurface.rs viewporter.rs
    xdg_shell.rs seat.rs output.rs data_device.rs activation.rs
    layer_shell.rs toplevel_management.rs
  window/                 Win2000 窗口策略的唯一 owner
    stack.rs focus.rs interactive_move.rs switcher.rs
```

### 进程拓扑

```
kernel boot_screen ─► BusyBox init ─► rcS
                        ├─ respawn /bin/compositor
                        │    └─ WAYLAND_SOCKET spawn /bin/logon（特权）
                        │         └─ 认证后以用户身份 spawn /bin/desktop（特权）
                        │              └─ spawn 应用 / wine，连接 $XDG_RUNTIME_DIR/wayland-0（普通）
                        ├─ respawn network-service
                        └─ UART /bin/sh（恢复路径）
```

权限边界：只有 compositor 用 `WAYLAND_SOCKET` 直接拉起的 `logon` 和由它交接的 `desktop` 能看到
`layer_shell` 与 `toplevel_management`。普通客户端看不到这两个全局对象。

### Wayland 协议清单

| 协议 | 用途 | 使用者 |
|---|---|---|
| `wl_compositor` v6、`wl_subcompositor`、`wl_shm`、`wl_seat`、`wl_output` v4、`wl_data_device_manager` | 核心 | 全部 |
| `xdg_wm_base` | 顶层窗口 | 全部 |
| `wp_viewporter`、`zxdg_output_manager_v1` | Wine | Wine |
| `xdg_activation_v1` | 启动后激活 | desktop、应用 |
| `zwlr_layer_shell_v1` | 桌面层、任务栏、登录屏 | 仅特权客户端 |
| `zwlr_foreign_toplevel_management_v1` | 任务栏按钮、激活、最小化、关闭 | 仅 desktop |
| `zwp_pointer_constraints_v1`、`zwp_relative_pointer_v1`（后续） | 游戏锁定鼠标 | Wine 游戏 |
| `zwp_linux_dmabuf_v1`（后续） | GPU buffer | Wine D3D |

Wine 的必需集合在阶段 0 用 `WAYLAND_DEBUG` 确认后定稿。

## 扩展点（借鉴 Windows 2000 的可替换性，分三层）

Win2000 的 DLL 同时承担四种需求，这里分别用不同机制满足，不引入进程内插件：

1. **数据层（现在做）**：主题、素材包、字符串表在运行时按"用户 > 本地原版 > 仓库默认"的顺序查找，换样式不重新编译。
   对应 Win2000 的资源 DLL（`shell32.dll` 图标、`msgina.dll` 位图）。`classic-theme` 的颜色、尺寸做成运行时加载的数据文件，
   而不是编进二进制；素材按逻辑名取，不感知来源。
2. **进程层（现在做）**：`logon`、`desktop`、控制面板小程序、屏保都是独立程序，靠配置路径和标准协议对接，换二进制即可替换。
   对应 GINA（`msgina.dll`）、`Shell=` 注册表项、`.cpl`、`.scr`；崩溃隔离优于进程内 DLL。
   控制面板小程序用清单文件声明名称、图标、命令，控制面板只负责枚举与启动。
3. **库层（暂缓）**：`classic-ui` 静态链接进每个应用。真正的共享库只能走 C ABI（Rust 没有稳定 ABI），是新增私有 ABI 契约、
   unsafe 与共享崩溃域，且本项目 `panic=abort`。只有出现第二个实现的需求再做，届时用版本化 C ABI 并指定唯一 owner。

## 实施方式

2026-10-24 起改为边实现边对齐规格：规格中"待核"的数值，在实现对应组件时用原版素材（`.local/inputs/windows2000/assets/`，
由 `scripts/extract_windows2000_assets.py` 提取）核对并改回规格，不再单独做一轮规格验证。

## HiDPI

1. compositor 按输出的物理尺寸选择**整数** scale（3008×1692 → 2，逻辑桌面 1504×846），通过
   `wl_output.scale` 和 `wl_surface.preferred_buffer_scale` 下发。分数 scale 不在本期范围。
2. `classic-theme` 只定义逻辑像素。1 逻辑像素的斜边线、边框在 scale N 下画成 N 物理像素，严格保持
   Win2000 的几何比例。
3. 文字不放大位图：1x 用 12 像素点阵的宋体替代字体，2x 及以上按物理字号用轮廓字体渲染；字体选择与许可证
   唯一定义在 `docs/design/classic-desktop/assets.md`。
4. 图标同时提供 16/32 和 32/64 px 两套，光标 32 和 64 px。硬件光标上限 64×64，scale 2 正好够用。
5. Wine：`LogPixels = 96 × scale`。非 DPI-aware 的老游戏由 Wine/compositor 缩放。

## 100% 还原的验收依据

新建持久设计文档 `docs/design/classic-desktop/`（阶段 1 产出），作为外观与交互的唯一验收标准：

- **外观规格**：系统颜色表、各控件尺寸、标题栏渐变、字体与字号、图标规格。
- **交互清单**（逐条验收），至少包括：
  - 窗口：双击标题栏最大化，Alt+Space 系统菜单，最小化/还原的标题栏动画，拖动时显示窗口内容。
  - 菜单：淡入和滑出动画，键盘导航，带下划线的快捷键（按 Alt 后才显示）。
  - 开始菜单：个性化菜单（折叠不常用项）、拖动排序、"运行"、"关闭 LiteOS"。
  - 任务栏：快速启动、托盘、气球提示、自动隐藏。
  - 桌面：图标拖放和排列、右键菜单、"显示属性"。
  - 快捷键：Win 键、Win+E、Win+R、Win+M、Alt+Tab 图标网格、Ctrl+Alt+Del。
  - 其他：工具提示的淡入、光标阴影。
- **核对依据**：公开 Win32 规范与用户提供的原版静态资源。不再搭建 Windows 2000 参照机；未核实项保留标记。

## 步骤 → 验证点

| # | 步骤 | 验证点 |
|---|---|---|
| 0 | **参照 spike（已验证，见阶段 0 进展）**：Alpine aarch64 上运行 Wine + FEX 与 Win32 探针 | 产出 syscall 缺口、Wayland 必需集合与运行时构建配方 |
| 1 | **（进行中，见"阶段 1 进展"）** 设计规格与素材：`docs/design/classic-desktop/`，`assets/classic/`（字体、1x/2x 图标、光标、开机画面、壁纸） | 规格评审通过；素材尺寸和格式由校验脚本检查 |
| 2 | **（内核侧已实现，见"阶段 2 进展"）** kernel `boot_screen` 与进度里程碑 | `verify_boot` 截图：第一帧即为 Win2000 风格的黑屏加进度条 |
| 3 | kernel 混音（`engine` / `mixer` / `control`），删除 `audio-service`、`audio-proto` 及其决策文档 | 单元测试；多路同时播放的音频分析 gate；`amixer` 可调节音量 |
| 4 | 用户态库：`wayland-wire`、`classic-theme`、`canvas`、`typeface`、`classic-ui` | codec 单元测试；黄金截图测试（1x 和 2x）；canvas 与字形 benchmark |
| 5 | **新会话实现**：compositor 与应用使用 Wayland，范围见下文 | 上游客户端一致性测试、完整会话 UI runtime gate 与 `make verify` |
| 6 | 按阶段 0 的清单补齐 kernel ABI，打包 `/opt/wine`；`classic-theme` 生成 Wine 前缀注册表 | 在 LiteOS 上运行 notepad、winemine、winecfg 和 ddraw/dsound 样例；Wine conformance 测试（`user32`、`gdi32`、`ddraw`、`dsound` 的 `*_test.exe`）结果与参照 Linux 一致 |
| 7 | 红色警戒 2（用户放入 `RA2_DIR` 后）：cnc-ddraw、开始菜单 `.desktop` 条目 | 进入遭遇战且有声音（runtime gate） |
| 后续 | `drm/virtgpu` 多 context + dmabuf + Mesa → D3D | 另立计划 |

阶段 4 从独立基础库推进；旧图形栈已拆除，阶段 4/5 从 UART 基线实现新会话，不恢复旧实现。

阶段 5 实现范围：新 compositor、`logon`、`desktop`、`file-explorer`、`command-prompt`、
`task-manager`、`media-player`。旧 UI、应用、音频服务与 seat 私有协议均已移除，不作为实现参照。
使用上游 `wayland-info` / `weston-simple-shm` 做一致性测试，UI runtime gate 覆盖开机 → 登录 → 桌面 → 打开各应用。

## 阶段 0 进展（参照环境：Alpine 3.24.2 aarch64，QEMU HVF，工程内 `.local/spikes/classic-desktop/`）

已验证：

1. **Win32 运行时组成**：Wine 11.16 源码，配置 `--enable-archs=aarch64,i386 --with-mingw=clang`，打上两组补丁：
   - Alpine 的 `rpath.patch`：musl 动态链接器不会按 soname 复用已加载的 `ntdll.so`，unix 侧 `.so` 缺 `$ORIGIN` rpath 时进程初始化中止。
   - Hangover 11.16 的 8 个提交（AndreRH/wine@3ea3a149，不含版本字符串提交）：缺少时 `syswow64` 无法生成，i386 进程加载 `kernel32.dll` 失败。
   - x86 模拟器用 Hangover 发布的 `libwow64fex.dll`（ARM64 PE，与 libc 无关），通过 `HODLL=libwow64fex.dll` 选择。
   - **musl 可用，不需要 glibc。**
2. **标准 Win32 程序**：i386 的 notepad、winemine、winecfg 在 labwc（wlroots headless + pixman）上经 winewayland 正常显示。
3. **DirectDraw**：自写探针按红警 2 的用法（独占全屏、`SetDisplayMode(800, 600, 16)`、翻转链、Lock/Flip）在 Wine 自带 ddraw 和 cnc-ddraw v7.1.0.0（GDI 渲染）上都能出图。cnc-ddraw 自带红警 2 的配置段（`[game]`、`[gamemd]`）。
4. **Wayland 协议**：winewayland 必需 `wl_compositor`、`xdg_wm_base`、`wl_shm`、`wl_subcompositor`、
   `wp_viewporter`，输入需要 `wl_seat`；可选但需要的有 `zwp_pointer_constraints_v1`、
   `zwp_relative_pointer_manager_v1`、`zwp_text_input_manager_v3`（中文输入法）、
   `zwlr_data_control_manager_v1`（剪贴板）、`xdg_toplevel_icon_manager_v1`（窗口图标）、
   `wp_fractional_scale_manager_v1`（HiDPI）。全屏使用 `xdg_toplevel.set_fullscreen`。
5. **syscall**：GUI 程序共用 66 个 syscall（`userfaultfd` 在 Alpine 上也返回 `ENOSYS`，Wine/FEX 能降级）。对照 LiteOS 代码核实：
   - 已具备：`MAP_FIXED_NOREPLACE`、`user.*` xattr（Wine 的 `user.DOSATTRIB`/`user.WINEREPARSE`）、
     `membarrier` private expedited（FEX）、aarch64 信号帧 FP/SIMD 上下文、普通文件未知 ioctl 返回 `ENOTTY`。
   - 与 Linux 一致可保留：`PR_SET_PTRACER`、`PR_SET_VMA` 返回 `EINVAL`（Linux 未启用 Yama / `ANON_VMA_NAME` 时同样如此）。
   - **缺口**：`prctl(PR_SET_NAME/PR_GET_NAME)`（应与 `/proc/<pid>/task/<tid>/comm` 共用 owner）、`fadvise64`。
   - 运行时待验证：在 `0x10000`、`0x110000` 处用 `MAP_FIXED|MAP_NORESERVE` 预留约 1.7 GB 的 `PROT_NONE`
     区域；`tkill` + `rt_sigreturn` 的线程挂起路径。

6. **声音**：参照虚拟机加 virtio-sound（与 LiteOS 同一声卡）后，探针的 DirectSound → winealsa → alsa-lib → ALSA
   链路正常。Wine 用读写模式（`WRITEI_FRAMES`），不用 mmap 模式；status/control 页 mmap 在 aarch64 上返回 `ENXIO`，
   alsa-lib 改用 `SYNC_PTR`。对照 `kernel/src/audio/ioctl.rs` 的**缺口**：
   - PCM：`HW_REFINE`（alsa-lib 参数协商的核心，必需）、`INFO`、`TTSTAMP`、`USER_PVERSION`、`REWIND`、`RESET`。
   - 整个 `/dev/snd/controlC0`：`CARD_INFO`、`PVERSION`、`PCM_INFO`、`PCM_NEXT_DEVICE`、`PCM_PREFER_SUBDEVICE`
     （由计划中的 `audio/control.rs` 承接）。
7. **Wine conformance 基线**（工程内 `.local/spikes/classic-desktop/out/baseline/*.tsv`，逐子测试记录退出码、执行数、失败数、跳过数）：
   - **aarch64 原生层**（不经 FEX，直接对照内核 ABI，LiteOS 的主要比较对象）：kernel32 33 个子测试、ntdll 27 个、
     gdi32 14 个、user32 24 个。`ntdll:exception` 在参照环境本身不稳定（同条件连跑 3 次：超时 / 全过 / 182 个
     调试器断点类失败），其子进程测试块稳定全过；比较规则为子进程块必须全过、主块只允许"调试器断点"类失败或超时。
   - **i386 层**（经 FEX）：`kernel32:{debugger,loader,process,sync}`、`ntdll:{exception,info}` 在 900 秒下仍挂起。
     其中 loader、sync 已确认是 Wine/FEX 的"子进程在仍有其他线程时无法退出"问题，红警 2 退出时可能遇到。
     `ntdll:wow64` 的 130 个失败来自测试假定 Windows 原生 `xtajit.dll`。
   - 两层都有的已知失败：`user32:input` 等依赖输入设备（headless 没有 `wl_seat`），`ddraw1/2/4/7` 依赖 3D 设备
     （无 GL，大量跳过后卡死）；`ddraw:ddrawmodes`（显示模式切换）427 项全过。
   - 比较规则：LiteOS 上的结果与基线逐子测试一致即为达标，多出的失败或挂起才归因于 LiteOS。

阶段 0 遗留：

- 构建前需要补 `gettext`，否则 Wine 没有中文界面翻译。
- 参照脚本（`.local/spikes/classic-desktop/scripts/`）在阶段 6 转为仓库内的 `scripts/wine_runtime.py` 与 runtime gate。

设计待定：

- Wine 的 800×600 全屏模式在 1504×846 输出上被非等比拉伸成约 1325×846；全屏缩放策略（等比加黑边、整数倍）由我们的 compositor 决定。
- Wine 窗口图标经 `xdg_toplevel_icon_v1` 交给 compositor，但 `zwlr_foreign_toplevel_management_v1` 没有图标字段；任务栏如何拿到图标需要在阶段 1 定方案。

## 阶段 1 进展

已有准备：

1. 设计规格 `docs/design/classic-desktop/`：外观、开机登录、桌面外壳、交互清单、素材清单；每项标注"确定/待核"。
2. 品牌规则：界面文案不出现"Windows"（微软商标），统一替换为"LiteOS"，交互与文案结构保持不变。
3. Codex 生成的素材（源文件在 `assets/classic/`，prompt 存于工程内 `.local/spikes/classic-desktop/scripts/prompt-*.txt`）：
   - 图标 105 张（31 个三尺寸图标、2 个托盘图标、4 个消息框图标）。16 像素版第一次生成像是大图缩小所得、
     发灰发虚，已整体重做为逐像素绘制（平均 6 色、1 像素深色轮廓）。
   - 光标 54 张（11 个静态光标 + 两组 8 帧沙漏动画）；热点由程序核对：箭头类 (0,0)、居中类 (16,16)、
     `link` (6,0)。沙漏翻转帧已重做为刚体旋转。
   - 位图 8 张：`boot-logo`、`logon-banner`、`start-banner`、`ctrl-alt-del` 的 1x/2x。
4. 字体（`assets/fonts/`，选择依据、来源哈希与渲染规则见 `docs/design/classic-desktop/assets.md`）：1x 文泉驿点阵宋体
   12 像素、2x 文鼎 PL 明体 CN 轮廓（已删除来源存疑的点阵表并写入修改声明）、西文 Wine Tahoma；中文粗体以 1 逻辑
   像素叠印合成。字体必须作为独立数据文件在运行时加载。

待完成：

- 本地原版素材：提取 owner 为 `scripts/extract_windows2000_assets.py`，只写入 Git 忽略的 `.local/inputs/`；
  已核对全部桌面图标、默认光标、三类位图和声音/字体的来源，见素材清单。尚未接入产品 rootfs，留待阶段 4/5
  的唯一素材打包入口消费，不能把素材准备算作桌面已切换。
- 规格中"待核"项：按公开规范与原版静态资源核对，不依赖 Windows 2000 参照机；未核实项不提升置信度。

## 阶段 2 进展（内核开机画面）

已实现并验证：

1. **从第一屏开始**：内核把 GPU 初始化提前到 `memory::init()` 之后（设备探测顺序改了：`virtio::bind_display` 先建立首个 GPU，
   `EarlyDisplay` 令牌带到 `bind_platform_devices` 补注册中断），随后立即把 `LBS1` 启动画面画上屏幕，并在命名空间、
   文件系统/scheduler、设备、音频四个里程碑推进进度条到 400‰。在 QEMU（HVF，1504×846）里截屏确认：画面居中、1 倍、
   进度条停在 40%；换成本地原版画面后同一套代码画出原版启动画面，进度条精确落在原版的槽里。
2. **素材选择只有一个点**：`scripts/classic_assets.py` 决定用本地原版（`.local/inputs/windows2000/packed/`）还是仓库默认
   （`assets/classic/boot-scene.lbs`）；内核构建（`LITEOS_BOOT_SCENE` 环境变量，cargo 自动跟踪）与 rootfs 构建都经它取路径。
3. **共用解码**：新增 `boot-scene` crate（`no_std`、不分配内存），当前由内核消费、供未来 compositor 复用，8 个单元测试；内核侧 `boot_screen`
   模块有 6 个宿主测试（假帧缓冲），打包器与选择函数有 Python 测试。旧 compositor 的测试随实现撤下，不计作当前覆盖。
4. **删除被取代的旧实现**：Aurora 启动场景（`assets/bootlogo.xrgb`、`scripts/convert_bootlogo.py`、compositor 里的程序化
   背景与 logo 图层解析）整体删除，不保留第二套。
5. **顺带根治的既有缺陷**：`arch::aarch64::mmu::virtual_to_physical` 用 `then_some` 急切求值，对内核镜像高地址在开启溢出检查的
   构建里 panic（release 不检查所以一直没暴露）；改为惰性求值。
6. 门禁：`boot-scene` 加入 `verify-unit` 与两处 `clippy -D warnings`（AArch64 与 RISC-V）；架构围栏、接口基线、依赖契约已更新；
   `make verify-unit` 通过。

尚未做 / 已知限制：

- **compositor 阶段的进度条动画**：当前停在 400‰ 直到桌面首帧。旧 compositor 已移除，新 Wayland compositor 尚待阶段 5 实现；`LBS1` 与里程碑常量已为接续预留。
- `verify_boot` 的 UTM 路径覆盖 kernel 图形设备拓扑与 init 启动；完整桌面会话尚不存在，不能据此宣称 UI 验收通过。
- 原版进度条是"跑马灯"还是按比例填充尚未核实（目前按比例）。
- 两张 640×480 以外的位图（登录横幅、开始菜单侧栏）已生成并对齐尺寸，但还没有消费者。

## UART 基线与阶段 3 进展

旧 LiteUI、React 应用、用户态音频 mixer、旧 compositor、显示器文本 console 与 seat 私有协议均已撤下。
init 只监督网络和 UART shell，rootfs 不再构建或安装旧 UI，增量同步会清理已退休的系统文件并保留用户数据。
原版静态资源、经典桌面素材、kernel 开机画面和 DRM/ALSA/输入能力继续保留。

内核音频提供 8 子流 Engine、Master/逐流音量、control 事件与 opener TID 子设备偏好。
固定上游 amixer/alsa-lib 的八路六秒无 XRUN 播放、左右增益/静音/削波 WAV 分析已通过；
热路径的 host diagnostic benchmark 与目标 workload 分别裁决数学成本和当前实际播放能力。
阶段 3 的持久事实见音频架构、契约、ABI 矩阵和构建验证文档；完整 ALSA/Wine 兼容性不由该探针替代。

## 阶段 4 进展（用户态基础库）

- `canvas`：填充、斜边、水平渐变、棋盘抖动、XOR 反色与点状焦点框、预乘 alpha blit（整数放大）、覆盖率蒙版着色；17 个测试。
- `classic-theme`：主题是数据文件 `assets/classic/theme/classic.theme`（键名同 Win2000 注册表值名），编译期嵌入为默认，
  `Theme::load` 按 用户 > 本地原版 > 默认 的搜索顺序覆盖；`DrawEdge` 取色表；`wine_registry(scale)` 生成 Wine 前缀 `.reg`；7 个测试。
- `wayland-scanner` + `wayland-wire`：自研 XML 子集解析与代码生成；固定 Wayland 1.24.0 / wayland-protocols 1.45 的
  `wayland`、`xdg-shell`、`viewporter`（来源与 SHA-256 见标准基线）。wire 层有 `Reader`/`Writer`/`Framer`、fd 队列、
  4096 字节上限回滚；生成的 `Request`/`Event` 供服务端与客户端共用。其余协议在 compositor 需要时再加 XML。
- `typeface`：自研 BDF 点阵、sfnt（cmap 4/12、glyf 含复合、EBLC/EBDT）读取与灰度光栅，1x 用文泉驿点阵 + Tahoma 11 ppem 内嵌点阵
  （无抗锯齿），2x 及以上用轮廓；西文只取 ASCII 与 Latin-1/扩展 A/B，其余（含中文引号）走中文字体；CJK 仿粗体叠印；
  禁则换行；16 个测试含损坏字体不 panic。未引入 `swash`。
- `canvas`/`typeface` 的 benchmark 决策见构建验证文档（诊断，无阈值）。
- `classic-ui` 绘制层：`Painter` 按逻辑像素 × 整数 scale 绘制边框、按钮（常态/按下/默认/焦点/禁用）、复选框与单选框、编辑框、
  滚动条（含滑块几何 `thumb_span`、抖动轨道、按下反色）、分组框、工具提示、菜单与菜单栏、窗口外框与标题栏；18 个像素探针测试，
  `cargo run -p classic-ui --example gallery -- <scale> <out.ppm>` 渲染 1x/2x 对照图。修复了 `canvas::bevel` 在 scale>1 下斜接方块不整块的问题。

## 阶段 5 进展（compositor）

- `/bin/compositor` 由 `display-service` 经 init 监督（无 `/dev/dri/card0` 时安静待命）。第一版用 **CPU 合成到 dumb buffer**
  （不上 VirGL）：唯一 DRM master、双缓冲、接管内核启动画面（首帧与内核逐像素一致）、读取全部 evdev 设备、
  硬件光标（`LCS1` 光标集，`scripts/pack_cursors.py` 打包，仓库原创默认已提交）。HiDPI scale = 逻辑高度不低于 768 的最大整数。
- `make verify-runtime-display`：首帧逐像素比对 + 注入指针/按键。
- Wayland 服务端已实现：`wl_display`/`wl_registry`/`wl_callback`、`wl_compositor` v6（`preferred_buffer_scale`、`enter`）、
  `wl_shm`（argb/xrgb8888，`pread` 读取）、`xdg_wm_base` v3 toplevel（initial configure、ack 校验、错误码）、`wl_output` v4；
  协议状态机与 socket 层有 51 个进程内测试。窗口栈：层叠放置、点击激活置顶、新窗口获得焦点、Alt+F4 → `xdg_toplevel.close`。
- `make verify-runtime-wayland`：上游 `wayland-info`（Alpine `wayland-utils`）列出全部全局对象；`wayland-probe` 走完整 xdg 生命周期与五帧 frame callback；
  截图逐像素确认窗口位置与背景；Alt+F4 后桌面恢复。`weston-simple-shm` 因 weston-clients 依赖链过重（cairo/pango/EGL）未采用，改用探针。
- `wl_seat` v7（指针、键盘）已实现：焦点策略 `focus.rs`（指针 enter/leave/motion/button 带 frame，键盘 enter/leave/key/modifiers，局部坐标按输出
  scale 换算）；`keyboard::xkb` 从同一张键码表生成自包含的 XKB keymap，由 guest 里上游 libxkbcommon 逐键裁决（`xkb-check`）；
  `linux-uapi::recv_fds` 支持一次收多个 fd。键盘重复由客户端按 `repeat_info`（30/s、500 ms）自己做；Caps Lock 作为 locked 状态下发。
- 已知缺口：`wl_pointer.set_cursor`（客户端光标）、popup 与 positioner、`xdg_toplevel` 的 move/resize/maximize/minimize、
  `wl_data_device`、`wp_viewporter`/`zxdg_output_manager_v1`（Wine 需要）、layer-shell 与 foreign-toplevel（桌面外壳需要）、缩小客户端 scale > 输出 scale。
- 下一步：窗口交互请求（move/resize/maximize/minimize）与客户端光标，然后 `logon`、`desktop`（需要 layer-shell 与 foreign-toplevel）。

- 待做：`classic-ui` 的窗口/输入层（Wayland 客户端、焦点与键盘导航、助记符下划线、菜单动画与交互）、黄金截图门禁。

## 风险

- （已解除）FEX / Wine 在 musl 上可用，见阶段 0 进展。代价是要跟随 Hangover 的 Wine 补丁集，升级 Wine 时必须同时升级 `libwow64fex.dll`。
- Wine 对 signal frame、低地址 `MAP_FIXED`、线程与 futex 的语义要求很严格，阶段 6 的工作量取决于阶段 0 的缺口清单。
- HVF 虚拟机里没有 Apple TSO 模式，FEX 必须用内存屏障模拟 x86 的强内存序，会有性能开销。红警 2 负载很低，预计可以接受。
- 红警 2 的光盘版需要 The First Decade 或 Origin 版本才能免光盘运行；最终取决于用户提供的版本。
