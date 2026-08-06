# 图形会话与 LiteUI 运行时（lite-runtime）契约

## Owner

- `lite-runtime` 是通用 GUI/JS 运行时**库**（crate `lite-runtime`，lib 名 `lite_runtime`），
  独占 QuickJS+React reconciler、CSS 渲染器、输入/定时器/剪贴板、场景与事件循环、通用文件桥
  `fs.*` 与音频播放管线（`media.*` + 边下边播 `GrowingFile` 流式解码）。每个 GUI 应用是链接该库的
  独立二进制 `/bin/<id>`（`desktop` / `file-manager` / `my-computer` / `terminal` / `music-player`），
  桌面 `apps.launch` 以该路径 spawn。应用专属 native 能力经 `HostExtension` 注入 `Host` dispatch
  级联（`invoke_media` 之后），不写进库：`music-player` 的 `MusicExt` 独占 QQ/网易云请求构造与
  eapi 加密、可选登录态文件（`/root/music-credentials.json`）、TLS/HTTP
  栈与流式下载 worker（唯一链接 ureq/rustls/ring 的二进制）；`desktop` 的 `DesktopExt` 独占启动器
  策略（`apps.list` 扫描 `app.json`、`apps.launch` 校验后 spawn `/bin/<id>`、`desktop.shutdown`）。
  `HostExtension` 只经 `ExtensionCx` 使用**通用**原语（`next_request_id`、`events().emit(channel,…)`
  worker→run-loop 事件、`register_stream`/`remove_stream` 流注册、`push_action`、`apps_root`），
  不得把应用语义漏进库。窗口**机制**（`desktop.surfaces/configure/move/focus/close`、accelerators、
  `audio-system.*` 音量）是 compositor 客户端基础设施，作为 `Role::Desktop` 能力留在库内，非应用策略。
- `compositor` 独占 DRM master/OFD、VirGL context/pipeline、evdev fd、scanout pair、page-flip state、
  client registry、GPU texture quota、last-presented scene、input routing、pointer capture、cursor
  position 与 session epoch。
  `compositor/spice_agent.rs` 独占 VDI/SPICE message assembly、monitor request 与 host clipboard state；
  `session/output.rs` 独占 output serial 与 connector-size publication；
  `compositor/session/accelerator.rs` 独占 global accelerator chord 表与 key-grab 状态机；
  `display-proto/accelerator.rs` 独占 `AcceleratorSet` wire codec。
- React desktop 独占 persistent window policy state。compositor 只保存已接受/已呈现 scene snapshot 与
  move-grab temporary transform，不得复制窗口位置、z-order、active/minimized/maximized policy。
- 每个 app connection 独占一个 top-level surface content revision；一个 OS process/QuickJS VM/React
  root 只对应一个 surface。desktop scene 独占 foreign surface geometry，两类 revision 不互相代理。
- `lite-runtime` UI thread 独占 QuickJS 与 mutable React host tree；render thread 只消费 immutable snapshot，
  独占 CSS/layout/text/raster cache。SPSC slot 与 snapshot arena ownership 必须线性转移，禁止共享 mutable tree。
- LiteUI 只支持 React；`react-reconciler` 是唯一 framework adapter。不得新增 DOM/ReactDOM、Vue adapter、
  framework-neutral virtual DOM 或第二套 scene builder。Web 标准要求只落在已声明的 CSS、事件、表单、
  clipboard 与 media 契约上，不构成浏览器兼容承诺。
- `lite-runtime` renderer 独占 CSS scroll offset、最新 scroll-port/scrollbar geometry 与 scrollbar drag；
  offset 只以 React host instance 的稳定 node id 寻址，节点消失时必须同步回收，应用不得复制该状态。
- `lite-runtime` input dispatcher 独占文档内 hover、pointer-capture target 与表单控件焦点；target/焦点
  只保存稳定 node id，每次事件必须从最新 hit snapshot 解析当前 listener。禁止 capture listener id，
  否则 React commit 替换 inline handler 后会把后续 motion/up 投递给已删除的回调。文本 `<input>`
  左键按下聚焦，键码经唯一 keymap（与终端共用键码表）转字符后以受控语义派发 `onInput` 新值；
  水平 `<input type=range>` 由同一焦点 owner 按 min/max/step 规范化 value，pointer down/drag 与方向键
  default action 派发字符串 `onInput`，disabled 控件既不聚焦也不派发。控制键仍先投递焦点节点的
  `onKeyDown`；无焦点时键盘退回全局 `onKeyDown`（终端/桌面 Escape）。文本光标与 range UA 外观都由
  `renderer/paint` 按同一焦点绘制；不新增 imperative focus state seam。
  pointer/click/wheel 必须从最深 hit target 沿稳定 host parent id 构造唯一冒泡路径，并在同一次
  `__liteDispatch` 中按 target→root 投递；`stopPropagation()`/`stopImmediatePropagation()` 必须阻止
  后续 ancestor。禁止用“所有包含该坐标的 listener”近似冒泡，否则重叠 sibling 会收到错误事件。
- `lite-runtime` 内部 owner seam 固定为 `input`（事件状态）、`input/dispatch`（DOM 冒泡与表单默认动作）、
  `renderer/gpu_paint`（帧布局与完整 immutable display list）、`renderer/retained`
  （文档/fixed identity、geometry 与 damage）、
  `renderer/layout/flex`（Flexbox longhand lowering）、`renderer/backdrop/kernel`（box blur kernel）、
  `style/selector`（选择器解析、specificity 与动态伪类匹配）、`display/allocation`
  （同步分配期间的协议推进）、`display/scene`（desktop flat-scene z-order/input 构造与原子提交）、
  `host/filesystem`（有界 list/read 与
  mkdir/remove/rename/copy，并提供 filesystem-backed `File` bridge，路径必须绝对、payload 有界，
  仅 app session）和 `audio`（worker/media state/decoder/service transport）。
  compositor 的 connection handshake/role assignment 只属于 `session/client`；这些子模块不得复制
  父模块持有的 session、renderer、display 或 host state。
- `quickjs-runtime` 是 QuickJS raw C ABI、unsafe、runtime/context、module loader、job queue 与 interrupt
  callback 的唯一 owner；其他 crate 不得声明 QuickJS extern、raw pointer 或复制 exception cleanup。
- `terminal-session` 独占 PTY child、VT state、scrollback、selection 与 dirty rows；React terminal 不得
  复制 parser/screen state。`ui/design-system` 独占 Aurora token、assets 与系统组件；应用不得复制窗口
  chrome、shell、菜单、表单、Sidebar、Toolbar、Dialog 或结构/状态图标样式；字符不得代替系统图标。
  app identity icon 与 filesystem content icon 必须使用不同资产，禁止把带 tile 的应用图标复用于文件对象；
  compositor 与 LiteUI 不读取主题。

## Interface

- `display-proto` 是唯一 graphical userspace IPC seam。握手版本必须精确相等并永久选择
  `HELLO_DESKTOP` 或 `HELLO_APP`；不得 capability negotiation、兼容消息或同连接角色切换。
- display-list frame 上限必须由 `MAX_DISPLAY_COMMANDS` 与单命令 `MAX_GLYPHS_PER_RUN` 的最大 wire
  长度精确推导，合法的高密度文本列表不得在外层 framing 被拒绝。compositor 收包与 LiteUI 编码只按
  header 声明/实际长度分配 heap storage；不得按该最坏上限建立栈数组或逐帧清零最大缓冲。
- scene 是不超过 64 KiB、最多 128 node 的完整 snapshot；session 最多 32 app surface。compositor
  必须先完整 decode/validate surface identity、configure serial、bounds、clip、input/opaque region、buffer
  ownership 与 quota，再原子替换 accepted scene；失败保留旧 scene。
- focused surface 在 scene 中声明，允许零个或一个；键盘 routing 只随 presentation 切换。不得增加
  imperative focus state seam。`<surface>` bounds 必须等于 adopted configure logical client size，禁止缩放。
- desktop scene 的 node 顺序即 z 栈：全屏 Pixels 底图先行，随后每个窗口先按其 frame clip 重绘桌面像素、
  再叠加其 foreign surface，并在其后放置该窗口后绘制 React chrome 的 empty-clip input-only Pixels node；
  overlay clip（Top Bar/Dock/系统面板）居末。同一桌面 buffer 可在多个 Pixels node 按 clip 复用；input-only
  node 不得写像素。窗口 Pixels 使用 outer border-edge mask；foreign surface 必须携带其 DOM 位置实际
  生效的完整祖先 CSS overflow clip chain，并额外携带所属 `WindowFrame` 的 canonical outer mask；不得以
  外圆角替代祖先 chain，也不得使用全屏方形 clip。每个 mask 保留
  padding-edge rect 与四角独立横纵半径，compositor 在唯一 VirGL fragment pipeline 中对所有 mask
  的交集施加亚像素 coverage。Y0-top texture sampling、VirGL 归一化 viewport 的 destination
  transform、layer rect 与全部 clip mask 必须共同投影到同一左上原点 window coordinate；禁止另用
  host rasterizer scissor 形成第二套坐标契约。
  每个窗口的 chrome 与 content 必须原子叠放，任何窗口内容不得覆盖自身 border ring、窗口圆角或其他
  窗口的 chrome，foreign surface 也不得截获 paint order 中位于它之后的透明 desktop hit target。
- desktop 与 app 共用唯一 retained document raster。结构、computed style、layout geometry、scroll 或
  backdrop dependency 变化声明完整 document damage；完整 geometry 不变且仅文本内容或受控
  `<input value>` 变化时，damage 是变化节点 border box 的并集。document 精确复用时，desktop damage
  只包含上一帧与当前帧的 `position: fixed` overlay clips（去重），从而同时覆盖出现、变化和移除。
  禁止把固定层或局部媒体进度变化退化为全屏 damage，否则并发 app surface commit 会阻塞 shell buffer
  release。
- retained display-list commit 必须显式携带 `base_revision` 与一个有界 physical damage rect；该 base
  来自 client 独占的 last GPU paint revision，不能用夹杂 scene commit 的 public revision 减一推导。非零
  `base_revision` 必须精确解析到该 owner 上一版 GPU target；compositor 先以 replacement blend 复制该
  revision、只清除 damage，再在同一 damage clip 下按原 CSS paint order 重放完整 display list。首帧与
  full repaint 的 base 为零；空 damage 只推进 metadata。禁止缺失 base 时退化为 full repaint，也禁止
  丢弃完整列表另建 move-underlay paint 路径。
- CSS `overflow: hidden/clip/auto/scroll` 的后代 raster 必须经过同一个祖先 clip stack；矩形 bounds 仅用于
  early reject 与 hit geometry，不能代替像素裁剪。`border-radius` 的 overflow clip 位于 padding edge，
  四角横纵半径分别扣除相邻 border，并以亚像素 coverage 同时约束背景、边框、阴影、文本、图片与 opacity
  group；opacity offscreen 与最终 composite 之间只能应用一次祖先 coverage。
- `<img>` 与 url `background-image` 必须由 `renderer/image` 的同一像素中心采样 owner 绘制；默认
  `image-rendering: auto` 使用预乘双线性过滤，显式 `crisp-edges`/`pixelated` 才使用最近邻。精确 1:1
  尺寸必须走单采样 fast path；图片自身圆角必须与其他 box primitive 一样使用 fractional coverage，禁止
  hard-skip 整数裁边或为 Dock、壁纸建立私有缩放路径。
- outer `box-shadow` 必须从偏移、spread 后的圆角 mask 计算边界内外连续 blur coverage，并排除原始
  border box；offset 只移动 mask，不能生成实心矩形阴影底板。GPU fragment 必须在一个 draw 中以
  圆角 signed-distance 计算有限三 sigma coverage，禁止稀疏二维 tap 产生块状阴影或重复填充窗口面积。
- app `SURFACE_COMMIT` 与 desktop `SCENE_COMMIT` 分别有 monotonic revision。frame latch 后到达的提交进入
  下一帧；`SURFACE_COMMIT` 空 damage 精确表示 retained pixels 未变化，首帧与 full repaint 必须显式
  携带全 surface rect。每连接最多 64 KiB nonblocking outbound queue。可合并 event 覆盖旧值，不可丢
  事件无法入队时断开连接；禁止 compositor writer thread。
- client 提交后不得同步等待 `PRESENTED`；`ACCEPTED` 只确认 compositor 原子接纳 revision，
  `PRESENTED` 只确认 page-flip completion。当前几何 generation 的 buffer 只能由 `BUFFER_RELEASE`
  重新变为 writable；output resize 后不再合法的 mapping 只能由 `BUFFER_RETIRED` 唯一、永久移除，
  两种事件不得互换。
  `DISCARDED` 是 output serial 被更新后该 revision 不会呈现的唯一 terminal acknowledgement；允许发生在
  `ACCEPTED` 前或后，client 必须结束该 revision，并按独立的 buffer 生命周期事件处理 mapping。
  双 buffer 都在途时 client 必须保留 latest-only dirty state，禁止排队栅格化旧 snapshot。
- buffer allocation 只经 compositor：每连接最多四个、session 最多十六个 full-frame equivalent，按
  `pitch * height` 计费，scanout 不计入。allocation failure 明确返回，不得抢占别的连接、降低尺寸或
  让 client 自行 CREATE_DUMB。DESTROY 只由 compositor 执行。retired GPU paint target 必须回到同
  owner、同尺寸的唯一 idle slot，后续 paint 复用该 resource；owner/尺寸改变时销毁，禁止每帧维护
  create/unref 兼容路径。
- compositor GPU effect target 由唯一 renderer pool 持有；capacity 固定为 opacity 最大嵌套及最深
  Gaussian clean-source/reduction/horizontal pass 所需的 `2 * MAX_DISPLAY_STACK_DEPTH + 3`。opacity
  backdrop/isolated target、backdrop blur 与 glyph blur 必须统一借还该 pool；任何 backdrop source 必须
  由当前 immutable display-list 的效果前缀重建，禁止读取 retained final target。blur 只走按半径降采样后
  的水平/垂直归一化 Gaussian pipeline；box-shadow 只能走同一 fragment pipeline 的 analytic coverage，
  不得另建 target。稳态 paint 不得同步 CREATE/ATTACH/GEM_CLOSE host resource。
- resize/maximize 使用 `CONFIGURE(serial)`；对应 app commit 进入 pending slot，直到 desktop scene 引用
  `CONFIGURE_READY(serial)` 才在同一 latch 切换 buffer 与 geometry。旧 pair 在 presentation 后释放。
  仅 foreign adoption/geometry 改变时 desktop scene 必须复用当前 pinned、只读像素 buffer，不得因此
  触发 React desktop 重栅格化。move 只允许由已投递 pointer-down serial 发起；compositor 对完整
  `windowGroup` 应用 bounded temporary transform。`MOVE_BEGIN` 到达后，compositor 才从当前 desktop
  display list 栅格化一个排除该 group 的只读 underlay，并将它作为非移动 GPU texture 经唯一 VirGL
  scanout pipeline 合成临时 transform；普通 scene/hover/key 不得预生成每窗口 underlay。授权 serial
  过期、group 消失或已有 grab 时立即回收该临时 target。pointer-up 返回最终 logical position，最终
  canonical scene 呈现后清除 grab；期间到达的新 scene 必须继承 transform，禁止跳回旧位置或保留
  canonical 残影。双 scanout 的 canonical state 禁止当作零偏移 move state 做局部重绘：每个 target
  首次进入 move 必须完整重建，只有相同 scene revision、相同 group 的连续 move state 才能按其
  target-local old/new transform 局部重绘。标题栏拖动只走 compositor grab，不得保留 React motion fallback。
- UTM 窗口 resize 只走 spice-protocol `VD_AGENT_MONITORS_CONFIG` → canonical CVT → 标准 DRM/KMS
  transaction；设备自身 config change 仍只走 `NETLINK_KOBJECT_UEVENT` group 1。compositor 对同一
  poll turn coalesce 后发布最新
  `OUTPUT_CONFIGURE(serial, physicalSize, deviceScaleFactor=2)`。`SCENE_COMMIT` 必须携带 exact output
  serial；新 serial 的 desktop triple buffer、双 scanout 与 KMS mode 作为一个事务接管，旧 front 在
  新 scene 完成 page flip 前保持 pinned。不得用宿主缩放、轮询 topology、断开 desktop epoch、固定
  1504×846 viewport 或双 renderer 路径模拟 resize。
- LiteUI document 的公开 viewport 只投影标准 `window.innerWidth`、`window.innerHeight`、
  `window.devicePixelRatio` 与 `resize` event；native output message 只更新这一个 platform owner。
  React desktop 与 app 必须消费该标准 viewport，禁止另设 Aurora/QEMU 私有 resize API。
- scene input region 是 compositor routing 的唯一依据，pixel alpha 不参与 hit-test。每 node 最多 64、
  整份 scene 最多 256 个 input rectangle；超限拒绝，不得扩大到 bounds。app surface 默认使用完整 client rect；
  desktop renderer 必须把同一窗口内位于 foreign surface 之后且可交互的 hit boxes 投影为后置 input-only
  node，以保持 CSS/DOM paint-order hit testing，禁止用缩小 app input bounds 或硬编码 resize inset 近似。
- pointer motion 只走 VirtIO-GPU queue 1 的 `MOVE_CURSOR` fast path；不得修改 scanout target、提交 VirGL
  draw、KMS page flip、等待 vblank 或同步等待 cursor completion。compositor 必须在每个完整 evdev
  `SYN_REPORT` 更新硬件位置；cursorq 固定单 slot，slot 忙时只覆盖尚未发布的 latest position。shape
  update 单独等待 exact completion 以保证 request DMA 与 resource 生命周期；cursorq 不得与 controlq
  render fence 共用水位。window move transform 仍最多一个 scene page flip 在途，期间只保留 latest
  transform。离散事件前必须先 flush preceding motion。
  button/key/wheel/focus 不可合并。capture 只能消费同一次 pointer-down 的 input serial，并在 up、unmount、
  focus loss 或 disconnect 时由 compositor exactly-once reset。
- cursor shape wire value 固定为 arrow、pointer、NS、EW、NESW、NWSE 与 hidden 七种；LiteUI 从标准 CSS
  `cursor` 值归一化，compositor 独占 checked 预乘 RGBA cursor asset（48×48 物理像素，`.lc2`）；每个
  asset 必须携带并校验自己的物理 `hot_x/hot_y`，禁止 compositor 按 shape index 猜测热点。切换时
  透明补齐到唯一 64×64 dumb BO，并由 DRM 以标准 B8G8R8A8 2D resource upload 后保留 hotspot；hidden 不创建平行透明 asset，而以
  `UPDATE_CURSOR(resource_id=0)` 禁用硬件光标。未知值必须回落 arrow，
  不接受应用 URL、位图或 theme asset。
- compositor 独占 pointer-focus surface 并据此仲裁 cursor 归属：`SetCursorShape` 仅在请求连接身份
  与当前 pointer-focus surface 一致时生效（surface_id 与连接不符视为协议错误）。pointer focus 切换
  或消失（含 capture 释放、app 断开、epoch reset）时先回落 arrow，新 target 在其首个 Motion 上报当前
  CSS cursor。LiteUI 不缓存全局 cursor owner；scanout 是形状去重与 cursorq publication 的唯一 owner。
- 非默认 CSS cursor 不依赖事件 listener 才进入 hit tree；LiteUI 保存最近一次 compositor-routed
  pointer position，并在每次 DOM/style scene 重建后重新求值 cursor。缺少该重算会让已卸载或
  `pointer-events: none` 的节点继续支配静止指针，直到下一次物理 motion。
- compositor 必须把 evdev wheel detent 转为有符号 logical CSS pixel delta。LiteUI 先投递同值 wheel
  listener，再执行 scroll default action；`overflow: hidden/clip` 只裁剪且不响应 wheel，
  `overflow: auto` 仅在实际 overflow 时显示 scrollbar，`overflow: scroll` 始终显示。短内容的 offset
  必须为零；content/viewport 缩小时必须 clamp；嵌套容器只把本层未消费 delta 传播给 ancestor。
- global accelerator table 由 desktop 经 `AcceleratorSet`（kind=26，`count` + `count × {modifiers, code}`，
  不超过 `MAX_ACCELERATORS=16` 条，空表清空）原子提交；app session 发送按协议错误处理。compositor
  只做 fixed chord 精确匹配（modifier mask 精确相等，repeat 不触发）：命中后进入 key grab，grab 期间
  全部 key 事件（含 modifier 自身变化与无关 key）路由 desktop（surface_id=0），chord 的全部 key 松开
  （次序无关）才结束并恢复 focused surface 路由；desktop 断开或 epoch reset 强制结束。modifier mask
  位定义固定为 Shift=1、Ctrl=2、Alt=4、Super=8。窗口 policy 与 shortcut action 不得进入 compositor。
- compositor 的 `spice_agent` 是 monitor/clipboard capability、session clipboard 与 SPICE vdagent
  transport 的唯一 owner。VDI client port 承载 monitor/clipboard，VDI server port 的标准 13-byte
  mouse-state 只做 framing 校验后丢弃；UTM/QEMU 必须以 `agent-mouse=off` 禁止 host 把 motion 改投该
  channel，真实 pointer input 只来自 compositor 独占的 evdev/tablet，不建立 SPICE input 第二路径。
  缺失该启动参数会让 SPICE client mode 截走物理 motion，而 canonical tablet 永远无事件。只接受单 monitor、
  原点为零、32-bit/unspecified depth 与不超过 8192
  的 geometry；水平尺寸按 Linux CVT 8-pixel cell 归一化，多显示器或 malformed payload 直接拒绝。
  display protocol 的 read/write/data 必须携带 exact surface/request identity，单次只接受
  不超过 60 KiB 的 UTF-8 text；host 数据按需请求，异步结果只能返回仍存活的原请求连接。
  LiteUI 只投影标准 `navigator.clipboard.readText()`/`writeText()` 与文本输入/Terminal 快捷键，
  不保存平行内容。无 image/file/HTML/primary selection、文件拖放或私有 path clipboard。
- QuickJS 每个 host→JS turn 使用固定 interrupt-check budget；Promise jobs 与 microtask 共用该预算。
  desktop heap 32 MiB、app heap 16 MiB、VM stack 512 KiB。超限是 fatal；native host call 必须非阻塞。
- 同一 JS turn 内同步 React mutation、job drain 后最多产生一个 revision；离散 host callback 必须进入
  reconciler 的同步事件边界并在返回前 commit，不同离散 input 不跨 turn
  合并。CSS timeline 只在 `PRESENTED` 后把活动 document 标 dirty，`ACCEPTED`、release 与 JavaScript
  timer 不得代替 refresh driver；snapshot arena 不可用时只记录 dirty，归还后从最新 host tree 生成。
- app entry 必须 default export 一个 component。target loader 仅接受固定 React/LiteUI system module；
  `lite:apps`、`lite:desktop` 与 `lite:audio-system` 必须拒绝普通 app session。应用可通过标准
  `<audio>` 与 `lite:fs` 的 filesystem-backed `File` 播放；native plugin、dlopen、应用自建 worker
  与 Node API 不存在。
- terminal helper stdin/stdout 使用长度前缀 binary protocol，stderr 只诊断。screen update 按完整脏行，
  cell metadata 标记“已写入”状态与宽字符 continuation，并携带 DECSCUSR 的 block/underline/bar 与 blink 状态；已写入状态区分原文空格与未使用空白，避免软换行重排吞掉行尾内容；
  update header 的前景/背景是 palette 7/0 的终端默认色，不能使用任意分片边界处 parser 的当前 SGR
  rendition，否则 TUI 尚未发送 reset 时会把整个未占用 viewport 错染为瞬态颜色；
  最多一个 update 在途，ACK 前变更合并；
  terminfo 同时发布通用 `Ss`/`Se` 和 Vim 外部 terminfo loader 使用的 `SI`/`EI`；resize 发送完整
  grid。helper argv 必须在 `--` 后显式给出，不提供默认 shell或 command-string parser。
- Clipboard API native host call 只能排队 nonblocking display request；compositor 的 focused-surface
  check 是唯一授权点。文本框 native paste 必须保存 node/request identity，异步结果若焦点或 node
  已变化就丢弃；Terminal paste 只写既有 PTY input protocol，不建立 clipboard-specific helper seam。

## Failure and cleanup

- desktop disconnect 是 session epoch terminal transition：compositor 关闭全部 app socket、撤销 capture、
  release/destroy 全部 client buffer 并保留 DRM owner；app 观察 EOF 后退出，init 重启 desktop。
- ordinary app uncaught exception、OOM、budget exhaustion、invalid dynamic style/asset 或 display protocol
  error 只终止该 app。desktop 的同类错误终止 epoch。LiteUI 只写 stderr，不显示 error page 或恢复 UI。
- close request 同步 unmount 唯一 React root、关闭 helper/fd、断开 display 并退出；应用不可 veto，也没有
  before-unload hook。PTY child exit 使 terminal-session 同码退出，React terminal 随后退出。
- compositor 必须在 connection teardown 沿唯一 owner path 撤销 pending configure/commit、scene
  references、clipboard request、accelerator sequence、pointer/key state 与所有 GEM mapping/handle。
  `POLLHUP/POLLERR` 是高于残留可读 frame 的 terminal transition；app teardown 必须先撤销其 pending
  paint、accepted/presented scene、routing、focus 与 grab，再释放 stream 和 GPU buffer。
  partial decode、allocation 或 SCM_RIGHTS failure 不得发布 resource identity。
- boot fallback 由 compositor 在取得 DRM 后立即一次性绘制，之后不运行 timer 或 progress animation。
  checked identity 资产只保存按最终物理像素生成的紧凑 logo/title/status premultiplied ARGB 图层，
  compositor 不缩放；它只承担 LiteUI 尚未连接或 desktop 失败时的静态品牌画面。desktop 首个完整
  scene latch 后由 React/CSS splash 原子接管；aurora、progress、hold 与 fade 只存在于 stylesheet，
  CSS timeline 的下一帧只由真实 page flip 驱动。不得恢复 compositor 动画、JavaScript timer/rAF
  动画或独立 splash 进程中的第二条实现。
- Splash 的 fixed overlay 必须由 CSS animation 在淡出 terminal frame 把 `display` 离散切到 `none`；
  renderer 对 `display:none` 不得生成 paint、hit 或 compositor overlay。只把 opacity 设为零仍会让
  全屏 desktop overlay 排在 foreign app 之后，导致应用 client surface 被永久覆盖。
- build-time 可验证的 manifest/CSS/bundle/asset error 必须阻止 rootfs 发布。runtime 不得 silent ignore、
  placeholder、旧协议 fallback 或降级 renderer。最终产品树不得保留旧 Rust shell/terminal renderer、
  旧 display protocol、atlas、`startmenu.conf` 或 `/bin/splash`。

## 性能契约

- first-class gate 是 AArch64+HVF、3008x1692、2 GiB、60 Hz，场景为 desktop、terminal 与第二窗口。
  window drag、菜单、scroll、terminal output、text input 与 background timer 的 frame p95 不超过
  16.67 ms、p99 不超过 33.3 ms，input-to-visible p95 不超过 33.3 ms。
  该 frame 预算由 `scripts/verify_frame_timing.py` 经 compositor 的 guest-vblank `compositor: frame-stats`
  marker 强制（见 build-and-verify 性能测试段），不再仅由契约文本维护。
- idle 不允许 render/commit/periodic wake；steady renderer/compositor frame 不允许 allocation。
  compositor+desktop+两个 app 总 RSS 不超过 256 MiB。RISC-V TCG 只承担正确性，不承担 60 Hz gate。
- scroll wheel hot path 只更新稳定 node id 对应的 offset；scroll region、scrollbar 与 active-id collection
  必须跨 frame 复用 capacity，UA scrollbar paint 只覆盖固定宽度 track/thumb。无需新增孤立
  microbenchmark，现有真实 scroll 场景的 frame/input-to-visible gate 是该路径的性能 owner。
- compositor 的一个 VirGL render pass 在 64 KiB execbuffer 上限内只能提交一次 setup/draw/teardown
  command stream；仅 command 超限允许有界切分。每个 referenced GEM 的 last fence 是异步资源生命周期
  owner，禁止在 pass 内按 setup、draw、teardown 或 layer 同步 wait。VirtIO-GPU controlq command owner
  按 queue descriptor capacity 的一半固定持有 slot（每条 command 正好消费 request/response 两个
  descriptor），保存每条 request/response/head/fence，使完整 effect paint burst 可有界异步提交；used
  completion 必须按 descriptor head 精确认领，DRM 只在更早公开 fence 都完成后推进 waiter 水位。2D
  modeset/damage resource transaction 仍由唯一 operation owner 串行，禁止保留 single-pending fallback。
- cursorq request/completion、move damage accumulation、wake descriptor 与 window-move page-flip pacing 使用
  固定容量状态；持续 motion/拖动不得按 evdev sample 排队 frame 或创建临时 collection。
- 视觉还原不属于自动 gate，不生成 preview screenshot 或 Golden；真实启动后的外观由人工裁决。
