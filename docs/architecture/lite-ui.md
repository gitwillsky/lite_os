# 图形会话与 LiteUI 当前架构

## 进程与 module

- `compositor` 是唯一 DRM master、evdev、scanout、page-flip、合成、输入路由与共享像素 buffer owner。
  它只理解物理像素、flat scene 和 surface，不理解 React、CSS、窗口策略或 XP 主题。
- `/bin/lite-ui` 是所有窗体程序共用的唯一 executable。每次启动建立一个进程、一个 QuickJS VM、
  一个 React root 和一个顶层窗口；desktop 使用唯一的 `--desktop` session，普通应用使用
  `--app <id>`。无窗体程序和 3D 游戏不经过 LiteUI。
- `quickjs-runtime` 是固定 QuickJS C ABI 的唯一 adapter，独占 Runtime/Context lifetime、ESM loader、
  Promise job drain、值转换、exception、heap/stack 与 interrupt budget。`lite-ui` 只消费其安全窄接口。
- React desktop 是 graphical session 的唯一窗口 policy owner：保存窗口位置、层级、active state、
  最小化/最大化、decorations、任务栏、开始菜单、壁纸、应用启动与 XP 产品呈现。
- `terminal-session` 是无窗体 helper，独占 PTY、VT parser、screen、cursor、scrollback 与 selection；
  React terminal 只绘制网格并转发输入、尺寸与 clipboard 操作。
- `ui/design-system` 是唯一 XP/Luna presentation owner。LiteUI theme-free，compositor 不包含窗口主题。

## 显示与调度

- compositor 与所有 GUI 进程只使用 `/run/display.sock`。握手把连接固定为一个 `Desktop` 或 `App`
  session；一个 compositor epoch 只允许一个 desktop。desktop 断开结束整个 epoch，compositor 回收
  全部 app connection/buffer，app 在 display EOF 后退出，init 从空 session 重启。
- desktop 提交完整 `SCENE_COMMIT`，拥有几何、层级、裁剪和 focused surface；app 独立提交
  `SURFACE_COMMIT`，拥有 surface 像素与 damage。page flip 前的单一 latch 点冻结两类最新 revision；
  输入始终命中最后一次已呈现状态。
- desktop renderer 的 flat scene 可交错 `Pixels` 与 `ForeignSurface` node。普通 app 只产生一个像素
  surface；desktop 遇到 `<surface>` 时切分 paint sequence，使窗口内容能与 React decorations 正确交错。
- LiteUI 像素使用预乘 `ARGB8888`，compositor 合成到双 `XRGB8888` scanout。每个 node 带保守的
  opaque region、显式 input region 与 damage；透明阴影不参与 input region。
- compositor 单线程 poll loop 独占 sockets、evdev、scene latch、damage composition、DRM page flip 与
  completion。LiteUI 使用 UI/render 双线程：UI thread 独占 QuickJS/React，native render thread 独占
  CSS、layout、text 与 raster。固定三个 snapshot arena 组成 latest-only seam，中间 revision 可丢弃。
- 每个像素 layer 严格双 buffer；静态 layer 可先持有一个 immutable buffer，首次变化时才申请第二个。
  compositor 接受 commit 后只读 front，旧 buffer 仅在 presentation 后 `BUFFER_RELEASE`。
- compositor 的双 scanout 分别记录最后 scene revision；复用 back scanout 时重画自该 revision 以来的
  damage 并集。damage 最多 64 个矩形，溢出合并为一个 bounding rectangle；epoch 或历史缺口才全屏重画。
- rAF 是 on-demand：可见连接最多一个 request outstanding，上一 page flip 完成后收到下一次 frame。
  完全遮挡或最小化的 app 不接收 rAF；后台 timer 最小 1000 ms，可见 app 最小 4 ms。无 revision
  不产生 render/commit，idle 不周期唤醒。

## React、CSS 与资源

- bundle default export 是唯一 React component，host 创建同步 mutation root。支持 hooks、context、
  fragment 与 keyed list；不开放 createRoot、portal、hydration、Server Components 或 concurrent root。
- host primitive 固定为 `<view>`、`<text>`、`<image>`、`<text-input>` 与 `<surface>`；controls 都是
  React component。desktop 用 `<view windowGroup={surface}>` 把 decoration 与 foreign surface 标为
  同一 compositor move group，不新增 `<window>` primitive。
- CSS 是严格标准子集：type/class/id/descendant/child selector，hover/active/focus/disabled，specificity、
  inheritance、variables、box、Flexbox、absolute、gap、min/max、background、border、radius、shadow、
  opacity、clip、z-index、text、`white-space`、overflow 与 `pointer-events`。不支持 Grid、float、table、
  pseudo-element、media query、filter、transition、CSS animation 或 vendor prefix；不支持项构建失败。
- layout 使用逻辑 CSS px，固定 `deviceScaleFactor=2`；默认 3008x1692 mode 对应 1504x846 viewport。
  LiteUI 是 logical/physical conversion 的唯一 owner。最终 box edge 从绝对逻辑坐标独立 snap 到物理像素。
- text 由 Parley shaping/layout，generic `monospace` 使用 JetBrains Mono 固定单格 advance；宽字符占两格，
  combining grapheme 附着前格，Noto CJK fallback 不改变 advance。字体只允许固定 sans-serif/monospace
  normal/bold。字形 cache 有界并使用 grayscale antialiasing。
- `<image>` 与 background 只接受 app-relative PNG 或 host 发出的 opaque `ImageSource`；路径必须在
  `assets/` 内且不能包含 `..`。SVG/JPEG/WebP 在 host build 转为 PNG；target 无网络、data URL 或动画图。
- raster 唯一使用 CPU tiny-skia，不建立 GPU backend abstraction。3D app 绕过 LiteUI。

## 应用与构建

- launchable app 位于 `/usr/share/liteos/apps/<id>/`，固定包含 `app.json`、`main.js`、`style.css` 与
  `assets/icon.png`；目录名必须等于 manifest id。desktop bundle 独立位于
  `/usr/share/liteos/desktop/`，不会进入应用 registry。
- host 以单一 `package-lock.json`、esbuild 和 `lite-ui-build` 构建 JS/JSX/TS/TSX；target 不包含
  Node/npm、dev server、HMR、runtime download 或 QuickJS bytecode。React/runtime module 只安装一次到
  `/usr/lib/lite-ui/`。
- target ESM loader 只接受固定 system bare specifier；项目相对 import 在 host 合并进 `main.js`。
  不支持 runtime relative/dynamic import、CommonJS、remote import 或 version negotiation。
- desktop-only `lite:apps` 扫描一层 registry，提供只读 metadata、opaque icon 与 `launch(id)`；
  desktop-only `lite:desktop` 提供 surface lifecycle/configure/close/move/accelerator mechanism。
  普通 helper 只通过 `lite:process.spawn(argv, stdio)`，不解析 shell string。

## 当前边界

- GUI 进程当前与 desktop 同等可信。握手仍共享同一 DRM OFD，但 dumb buffer 只能按协议向 compositor
  请求；compositor 是 CREATE/DESTROY owner，LiteUI 只 MAP_DUMB+mmap。权限模型和隔离后的共享内存
  transport 属于后续破坏性协议升级。
- input v1 只有 US keyboard、pointer、wheel、focus、repeat、text clipboard 与基础 keyboard
  accessibility；无 IME、dead key、layout switch、ARIA/screen reader、drag-and-drop、touch 或 app 自定义 cursor。
- 视觉还原不生成 screenshot preview 或 Golden，不进入自动门禁；最终由真实启动人工验收。
