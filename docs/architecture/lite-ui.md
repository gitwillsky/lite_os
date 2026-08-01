# 图形会话与 LiteUI 当前架构

## 进程与 module

- `compositor` 是唯一 DRM master、evdev、scanout、page-flip、合成、输入路由与共享像素 buffer owner。
  它只理解物理像素、flat scene 和 surface，不理解 React、CSS、窗口策略或 Aurora 主题。
- `compositor/clipboard.rs` 是 session plain-text clipboard 与 SPICE vdagent framing 的唯一 owner；
  它通过标准 VirtIO named port 与 QEMU host clipboard 懒交换数据，并只把结果路由到当前 focused
  surface。desktop 和 app 不保存另一份 system clipboard。
- `/bin/lite-ui` 是所有窗体程序共用的唯一 executable。每次启动建立一个进程、一个 QuickJS VM、
  一个 React root 和一个顶层窗口；desktop 使用唯一的 `--desktop` session，普通应用使用
  `--app <id>`。无窗体程序和 3D 游戏不经过 LiteUI。
- `lite-ui/main.rs` 只编排进程生命周期、渲染提交与 helper；`input.rs` 独占 input state，
  `input/dispatch.rs` 独占 Web-style DOM dispatch/default action，`renderer/render.rs` 独占帧布局与
  retained 选择，`renderer/retained.rs` 独占 document identity/damage/pixel reuse，
  `renderer/paint.rs` 独占递归 paint walk，`renderer/paint/fixed.rs` 独占 fixed layer traversal，
  `renderer/layout/flex.rs` 独占 Flexbox longhand lowering，`renderer/backdrop/kernel.rs` 独占
  allocation-free blur kernel，`style/selector.rs` 独占 selector/specificity/pseudo-class matching，
  `display/allocation.rs` 独占 buffer allocation round-trip，
  `host/filesystem.rs` 独占 filesystem-backed `File` 的只读文件系统 host bridge；`audio/` 独占每进程
  media worker、decode/resample、seek generation 与 audio-service transport。
- `compositor/session.rs` 保存 epoch 与 client registry；`session/client.rs` 只负责握手和连接角色固定；
  `session/output.rs` 独占 DRM hotplug 到 desktop output configure 的状态转换；
  `session/accelerator.rs` 独占 chord 匹配与 key grab（命中后完整 key sequence 只路由 desktop）。
  `scanout.rs` 只保留生产合成路径，其 white-box 测试位于 `scanout/tests.rs`。
- `quickjs-runtime` 是固定 QuickJS C ABI 的唯一 adapter，独占 Runtime/Context lifetime、ESM loader、
  Promise job drain、值转换、exception、heap/stack 与 interrupt budget。`lite-ui` 只消费其安全窄接口。
- React desktop 是 graphical session 的唯一窗口 policy owner：保存窗口位置与尺寸、层级、active state、
  最小化/最大化、decorations、Top Bar、Dock、Workspace Overview、Command Center、System Center、
  壁纸与应用启动。
- `terminal-session` 是无窗体 helper，独占 PTY、VT parser、screen、cursor、scrollback 与 selection；
  React terminal 只绘制网格并转发输入、尺寸与 clipboard 操作。
- LiteUI runtime 提供标准异步 `navigator.clipboard.readText()`/`writeText()`。受控文本框使用
  Ctrl/Cmd+C/X/V，当前 append-only caret 模型下 copy/cut 作用于完整 value、paste 追加到 value；
  Terminal 使用 Ctrl+Shift+V 或 macOS Cmd+V，把 UTF-8 文本作为一帧 PTY input。
- `ui/design-system` 是唯一 Aurora presentation owner：独占 token、窗口 chrome、系统 shell、菜单、表单、
  Sidebar、Toolbar 与 Dialog。应用只组合这些语义组件与业务内容；LiteUI theme-free，compositor 不包含窗口主题。
  `SystemIcon` 提供不依赖字体字形的排序、树形展开与状态图标；应用图标和文件系统内容图标是不同语义资产，
  前者使用 256px Aurora master，后者使用透明 256px 大图与 32px Retina 小图。

## 显示与调度

- compositor 与所有 GUI 进程只使用 `/run/display.sock`。握手把连接固定为一个 `Desktop` 或 `App`
  session；一个 compositor epoch 只允许一个 desktop。desktop 断开结束整个 epoch，compositor 回收
  全部 app connection/buffer，app 在 display EOF 后退出，init 从空 session 重启。
- desktop 提交完整 `SCENE_COMMIT`，拥有几何、层级、裁剪和 focused surface；app 独立提交
  `SURFACE_COMMIT`，拥有 surface 像素与 damage。page flip 前的单一 latch 点冻结两类最新 revision；
  输入始终命中最后一次已呈现状态。
- 标题栏 pointer-down 仍由 React desktop 判定窗口策略；它用同一 input serial 授权 compositor move
  grab，并一次性栅格化排除该 `windowGroup` 的 underlay scratch。后续 motion 只更新整个 group 的
  临时物理 transform，并在 front scanout 上用 underlay 重画旧/新 bounds 并集；pointer-up 返回最终
  逻辑坐标，下一份 canonical scene 一次性接管。motion 期间不运行 React、CSS layout、desktop raster
  或 page flip。
- desktop renderer 的 flat scene 可交错 `Pixels` 与 `ForeignSurface` node。普通 app 只产生一个像素
  surface；desktop 遇到 `<surface>` 时切分 paint sequence，使窗口内容能与 React decorations 正确交错。
- LiteUI 像素使用预乘 `ARGB8888`，compositor 合成到双 `XRGB8888` scanout。每个 node 带保守的
  opaque region、显式 input region 与 damage；透明阴影不参与 input region。React paint order 中位于
  foreign surface 之后的透明交互 chrome 生成 empty-clip `Pixels` input node，使 resize grip 等元素按
  标准 DOM z-order 命中 desktop，同时不复制 desktop 像素覆盖 app content。窗口 frame Pixels 使用
  outer border-edge mask；foreign surface 携带 renderer 当时完整的 CSS overflow clip chain，每层保留
  padding-edge rect 与四角横纵半径。compositor 逐 scanline 求所有 mask 的交集并合成一次亚像素
  coverage，因此 client content 不能用外圆角越过 inner border arc 覆盖边框。renderer 的同一 clip stack
  在 raster seam 约束其他 primitive。
- compositor 单线程 poll loop 独占 sockets、evdev、scene latch、damage composition、DRM page flip 与
  completion。LiteUI 使用 UI/render 双线程：UI thread 独占 QuickJS/React，native render thread 独占
  CSS、layout、text 与 raster。固定三个 snapshot arena 组成 latest-only seam，中间 revision 可丢弃。
- compositor 通过标准 `NETLINK_KOBJECT_UEVENT` group 1 与 display socket/evdev 同 poll；一次 resize
  burst drain 到 `EAGAIN` 后只查询一次最新 DRM topology，并发布 monotonic
  `OUTPUT_CONFIGURE(serial, physicalSize, scale=2)`。desktop 只有在新尺寸 triple buffer 与完整 scene
  就绪后才重建双 scanout、modeset 并 page flip；旧 output serial 的 scene 以 `DISCARDED` 终止。
  当前几何 generation 的 buffer 只通过 `BUFFER_RELEASE` 重新变为 writable，过期尺寸的 mapping
  只通过 `BUFFER_RETIRED` 永久移除；不重启 epoch、不让 QEMU 做模糊 host scaling，也不保留固定尺寸
  compositor 路径。
- desktop raster 把 document 与 `position: fixed` subtree 作为两个标准 paint phase。document layer
  只有在非 fixed host props、computed style、scroll offset 或 viewport 任一精确变化时才失效；纯 shell
  overlay commit 复用其完整像素、hit、window 与 scroll geometry，再按 CSS paint order 合成 fixed
  subtree。move-underlay 仍过滤后完整生成，不复用 presentation cache。
- app 与 desktop document 共用 retained raster/damage owner。布局与完整 computed style 不变时，仅
  文本内容或受控 `<input value>` 的变化恢复上一 revision 像素并按原 CSS paint order scissor 重画受影响
  border box；结构、布局、其他 prop/style、scroll 或 backdrop dependency 任一变化立即升级为完整
  document repaint。`SURFACE_COMMIT` 的空 damage 表示像素未变，不再暗含 full-buffer damage。
- 每个像素 layer 严格双 buffer；静态 layer 可先持有一个 immutable buffer，首次变化时才申请第二个。
  compositor 接受 commit 后只读 front，已呈现 desktop buffer 保持 pinned；只改变 foreign adoption
  或几何的 scene 可继续引用它而不重画像素。新像素 scene 呈现后才向 client `BUFFER_RELEASE` 旧 buffer。
- desktop 额外持有一个 full-size move-underlay scratch；它不进入普通 scene，也不形成第三条 presentation
  路径。grab 开始后 compositor 将其 pin 为只读，最终 canonical window scene 呈现后立即 release。
- compositor 的双 scanout 分别记录最后 scene revision；复用 back scanout 时重画自该 revision 以来的
  damage 并集。move grab 期间每个 scanout 额外记录移动窗口组最后绘制的 rect，下一次 full compose
  必把该 stale rect 并入 damage，否则并发 surface 提交会在 flip 后留下旧临时位置的残影。
  damage 最多 64 个矩形，溢出合并为一个 bounding rectangle；epoch 或历史缺口才全屏重画。
- LiteUI commit 只发送 revision 后立即返回，不同步等待 `PRESENTED`。两个 client buffer 都在途时保留
  最新 dirty host tree，任一 release 到达后只渲染一次最新状态；`ACCEPTED`、`PRESENTED` 与
  `BUFFER_RELEASE` 仍按 revision 校验，不能把异步节奏降级为无序提交。
- CSS refresh driver 是 on-demand：活动 animation/transition 的下一次采样只由上一 revision 的
  `PRESENTED(monotonic_ns)` 触发，`ACCEPTED`/`BUFFER_RELEASE` 只推进 buffer pipeline，不能推进
  document timeline。有限动画到达填充后的 terminal frame 即停止请求 render/commit，idle 不周期唤醒。
  JavaScript `requestAnimationFrame` 当前不对 bundle 开放，避免建立 timer 与 page-flip 两套时钟。

## React、CSS 与资源

- LiteUI 只支持 React，不实现 DOM/ReactDOM，也不承诺 Vue 或原生网页兼容。`react-reconciler` 是
  React 到 LiteUI host tree 的唯一 adapter；CSS、事件、表单与媒体在该 host tree 上遵循已声明的
  Web 标准子集，应用不得绕过它建立第二套 UI runtime。
- bundle default export 是唯一 React component，host 创建 mutation root。支持 hooks、context、
  fragment 与 keyed list；离散宿主事件在同一个 QuickJS turn 内同步提交，不能拖到下一输入事件。
  不开放 createRoot、portal、hydration 或 Server Components。
- host primitive 固定为 `<div>`、`<span>`、`<img>`、`<input>`、`<button>` 与 `<audio>`；
  `<input>` 支持受控文本框和标准水平 `type=range`：range 的 min/max/step/value 由 renderer
  规范化，pointer drag 与方向键默认动作派发字符串 `onInput`，UA 轨道/滑块由 renderer 绘制并消费
  标准 `accent-color` 计算值。
  `<audio>` 投影冻结的 HTMLMediaElement playback surface 与 UA controls，其他 controls 是 React
  component。desktop 用带 `data-lite-window`/`data-lite-surface` 的 `<div>` 把 decoration 与 foreign
  surface 标为同一 compositor move group，不新增私有 `<window>`/`<surface>` primitive。
- CSS 是严格标准子集：selector list、type/class/id/descendant selector、`:first-child`、`:last-child`、`:nth-child(An+B)`、
  `:hover`/`:active`/`:focus`/`:disabled`、specificity、inheritance、custom properties 与嵌套
  `var()` fallback、`inherit`/`initial`/`unset`、box、Flexbox、标准 margin 长度/百分比/`auto`、
  absolute、gap、min/max、
  background、border、radius、shadow、opacity、clip、z-index、text、`white-space`、overflow 与
  `pointer-events`。颜色经 cssparser 解析，支持 hex、`rgb()`/`hsl()`（legacy 与现代语法）、完整命名色与
  `transparent`（`currentColor` 不支持）。`border-style` 支持 solid/dotted/dashed 与双色斜面
  outset/inset/groove/ridge/double（亮/暗色由 border-color 按 UA 固定系数推导）。`box-shadow` 支持
  多层、spread 与 inset；outer shadow 对偏移后的圆角 mask 作双侧 Gaussian falloff，并从原始
  border box 扣除，禁止把 offset 区域作为实心底板。`background` 支持
  color/image/repeat/position/size 及简写（不认识的
  origin/clip/`fixed` token 忽略）；url 背景默认 intrinsic 尺寸 + repeat，`<img>` 仍拉伸填满。
  `<img>` 与 url 背景共享像素中心采样器；`image-rendering` 按继承属性支持
  `auto`/`smooth`/`high-quality` 的预乘双线性过滤，以及 `crisp-edges`/`pixelated` 的最近邻过滤。
  图片自身 `border-radius` 与 ancestor overflow clip 均以亚像素 coverage 合成，不允许整数裁边重新引入毛刺。
  `linear-gradient` 支持任意角度（对角 `to *` 关键字映射 45° 家族）。`backdrop-filter: blur()` 在
  rounded border box 内对已绘制 backdrop 作三次可分离 box pass；物理半径达到 16px 时在四分之一
  线性尺寸的抗混叠 backdrop 上执行同一 filter，再双线性恢复，避免 CPU raster 对大半径玻璃层作
  无效的过采样。filter result 按稳定 host node id 与完整输入像素精确保留；opacity 离屏 group 内的
  backdrop filter 明确拒绝，避免采样错误。`opacity` 是 group 语义：
  子树先离屏合成再整体按 alpha 混合。`pointer-events: none` 关闭整个子树的 hit/scroll 注册，
  不支持后代用 `auto` 重新开启。`box-sizing` 支持 `content-box`/`border-box`，但 UA 默认是
  `border-box`（偏离 Web 初始值 `content-box`），theme.css 全部按 border-box 编写。
  CSS Transforms 支持不改变 layout 的 `translate()`/`translateX()`/`translateY()`，paint、descendant
  coordinate 与 hit region 使用同一变换；CSS Animations 支持 `@keyframes` 的 `from`/`to`/百分比帧及
  单项 `animation` shorthand，CSS Transitions 支持单 property `transition` shorthand。数值、px 长度
  与 translate 在 presentation timeline 上插值，其他 property 按 discrete interpolation；`display`
  与 `none` 之间按 Web 离散特例在进入时的 0%/退出时的 100% 切换；
  timing function 支持 linear/ease/ease-in/ease-out/ease-in-out。media query 当前只接受
  `prefers-reduced-motion: reduce|no-preference`，平台尚无 reduce 设置时匹配标准默认
  `no-preference`。不支持 Grid、float、table、pseudo-element、其他 media query、filter、多项
  animation/transition、复合/scale/rotate transform 或 vendor prefix；不支持项构建失败。
- React host instance 在完整 snapshot 中携带稳定 node id；LiteUI renderer 以该 id 独占 CSS scroll
  offset，并让 hover/pointer capture 在 snapshot 重建后继续解析同一元素的最新 listener。
  事件从最深命中节点沿实际 host parent 链冒泡，`stopPropagation()`/`stopImmediatePropagation()`
  在同一次 JavaScript dispatch 内截断后续 ancestor；不得按几何包含关系把重叠 sibling 当成 ancestor。
  `overflow: auto/scroll` 形成通用双轴 scroll container，renderer 根据 layout content extent
  收敛 offset、移动并裁剪 descendant、绘制 overlay UA scrollbar；wheel delta 在嵌套 scroll port
  到达边界后向 ancestor 链式传播，应用无需保存私有 `scrollTop`。
- layout 使用逻辑 CSS px，固定 `devicePixelRatio=2`；默认 3008x1692 mode 对应 1504x846 viewport，
  QEMU Cocoa 把可调整窗口的 backing-pixel 尺寸作为动态物理 mode。LiteUI 是 logical/physical
  conversion 的唯一 owner；奇数物理边长对应最后一个部分覆盖的 CSS pixel，不另建 scale path。
  runtime 在应用模块求值前提供标准 `window === globalThis`、`innerWidth`、`innerHeight`、
  `devicePixelRatio` 与 `addEventListener("resize", ...)`。最终 box edge 从绝对逻辑坐标独立 snap 到物理像素。
- text 由 Parley shaping/layout、swash 运行时栅格化：任意 `font-size` px 生效，`font-weight` 数值按
  CSS Fonts 匹配映射到 subsetted Noto Sans regular/bold（`assets/fonts/liteos-ui-{regular,bold}.otf`，
  4111 codepoint，由 `scripts/generate_ui_font.py` 生成并发布到 `/usr/share/liteos/`）；
  `line-height` 支持 px/倍数/百分比，`white-space: normal/pre-wrap` 经 Parley line breaking 真换行。
  generic `monospace` 使用 JetBrains Mono 固定单格 advance；宽字符占两格，combining grapheme 附着前格。
  字形 cache 有界（LRU）并使用 grayscale antialiasing。
- `<image>` 与 background 只接受 app-relative PNG 或 host 发出的 opaque `ImageSource`；路径必须在
  `assets/` 内且不能包含 `..`。PNG 的 indexed/grayscale/grayscale-alpha/RGB/RGBA 输入统一规范化为
  8-bit 预乘 ARGB；SVG/JPEG/WebP 在 host build 转为 PNG；target 无网络、data URL 或动画图。
- `lite:fs.open(path)` 返回 filesystem-backed 标准 `File`；`URL.createObjectURL(file)` 只发布当前
  process 内 opaque `blob:` source。`<audio>` 只接受 app-relative resource 与该 `blob:` source，
  不接受 ambient `file:` path、network/data URL 或私有 path-play API。
- raster 唯一使用 CPU（盒/边框为手绘 scanline，字形为 swash A8），不建立 GPU backend abstraction。
  3D app 绕过 LiteUI。

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
  desktop-only `lite:desktop` 提供 surface lifecycle/configure/close/move 与 `setAccelerators(chords)`
  （全量替换 global accelerator table，不超过 16 条，空表清空；chord 命中后的完整 down/up sequence
  经全局 `onKeyDown` 到达 desktop）。desktop 注册 Alt+Tab（按当前 z-order 激活下一非最小化窗口）
  与 Alt+F4（关闭 active 窗口）。desktop-only `lite:audio-system` 只投影 audio-service master snapshot
  和更新请求，System Center 的音量与静音控件直接消费该唯一状态；普通 app 无法加载该 module。
  desktop 首次呈现固定启动 Files 与 Terminal；后续应用由 Command Center 或 Dock 启动。普通 helper 只通过
  `lite:process.spawn(argv, stdio)`，不解析 shell string。

## 当前边界

- GUI 进程当前与 desktop 同等可信。握手仍共享同一 DRM OFD，但 dumb buffer 只能按协议向 compositor
  请求；compositor 是 CREATE/DESTROY owner，LiteUI 只 MAP_DUMB+mmap。权限模型和隔离后的共享内存
  transport 属于后续破坏性协议升级。
- input v1 只有 US keyboard、pointer、wheel、focus、repeat、plain-text clipboard 与基础 keyboard
  accessibility；`cursor` 只支持固定 arrow、hidden、pointer 与四向 resize shape，不支持 URL/custom bitmap。
  非默认 CSS cursor 即使没有 React listener 也建立命中区域；host/style 重绘后会在最新 pointer position
  重新求值，因此元素消失或 `pointer-events` 改变不需要用户移动鼠标才能恢复正确 cursor。
  clipboard 单次最多 60 KiB UTF-8，不支持 image、file、HTML、primary selection 或 Finder
  drag-and-drop。无 IME、dead key、layout switch、ARIA/screen reader、drag-and-drop 或 touch。
- Web media 当前只提供标准音频播放，不提供 capture、Web Audio、MSE、MediaStream、remote playback、
  EME、track 或非 `1x` playbackRate；精确 codec 与状态边界见音频领域文档。
- 视觉还原不生成 screenshot preview 或 Golden，不进入自动门禁；最终由真实启动人工验收。
