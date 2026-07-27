# 图形会话与 LiteUI 契约

## Owner

- `compositor` 独占 DRM master/OFD、evdev fd、scanout pair、page-flip state、client registry、buffer
  quota、last-presented scene、input routing、pointer capture、cursor position 与 session epoch。
- React desktop 独占 persistent window policy state。compositor 只保存已接受/已呈现 scene snapshot 与
  move-grab temporary transform，不得复制窗口位置、z-order、active/minimized/maximized policy。
- 每个 app connection 独占一个 top-level surface content revision；一个 OS process/QuickJS VM/React
  root 只对应一个 surface。desktop scene 独占 foreign surface geometry，两类 revision 不互相代理。
- `lite-ui` UI thread 独占 QuickJS 与 mutable React host tree；render thread 只消费 immutable snapshot，
  独占 CSS/layout/text/raster cache。SPSC slot 与 snapshot arena ownership 必须线性转移，禁止共享 mutable tree。
- `lite-ui` renderer 独占 CSS scroll offset、最新 scroll-port/scrollbar geometry 与 scrollbar drag；
  offset 只以 React host instance 的稳定 node id 寻址，节点消失时必须同步回收，应用不得复制该状态。
- `lite-ui` input dispatcher 独占文档内 hover、pointer-capture target 与文本输入焦点；target/焦点
  只保存稳定 node id，每次事件必须从最新 hit snapshot 解析当前 listener。禁止 capture listener id，
  否则 React commit 替换 inline handler 后会把后续 motion/up 投递给已删除的回调。`<input>` 是唯一文本
  输入原语：左键按下聚焦，键码经唯一 keymap（与终端共用键码表）转字符后以受控语义派发 `onInput`
  新值，控制键（Enter/Esc/方向）投递焦点节点的 `onKeyDown`；无焦点时键盘退回全局 `onKeyDown`
  （终端/桌面 Escape）。文本光标由 `renderer/paint` 按焦点绘制；不新增 imperative focus state seam。
- `lite-ui` 内部 owner seam 固定为 `input`（事件目标与默认动作）、`renderer/paint`（递归绘制）、
  `display/allocation`（同步分配期间的协议推进）、`host/filesystem`（有界 list/read 与
  mkdir/remove/rename/copy，并提供 filesystem-backed `File` bridge，路径必须绝对、payload 有界，
  仅 app session）和 `audio`（worker/media state/decoder/service transport）。
  compositor 的 connection handshake/role assignment 只属于 `session/client`；这些子模块不得复制
  父模块持有的 session、renderer、display 或 host state。
- `quickjs-runtime` 是 QuickJS raw C ABI、unsafe、runtime/context、module loader、job queue 与 interrupt
  callback 的唯一 owner；其他 crate 不得声明 QuickJS extern、raw pointer 或复制 exception cleanup。
- `terminal-session` 独占 PTY child、VT state、scrollback、selection 与 dirty rows；React terminal 不得
  复制 parser/screen state。`ui/design-system` 独占 XP assets/theme；compositor 与 LiteUI 不读取主题。

## Interface

- `display-proto` 是唯一 graphical userspace IPC seam。握手版本必须精确相等并永久选择
  `HELLO_DESKTOP` 或 `HELLO_APP`；不得 capability negotiation、兼容消息或同连接角色切换。
- scene 是不超过 64 KiB、最多 128 node 的完整 snapshot；session 最多 32 app surface。compositor
  必须先完整 decode/validate surface identity、configure serial、bounds、clip、input/opaque region、buffer
  ownership 与 quota，再原子替换 accepted scene；失败保留旧 scene。
- focused surface 在 scene 中声明，允许零个或一个；键盘 routing 只随 presentation 切换。不得增加
  imperative focus state seam。`<surface>` bounds 必须等于 adopted configure logical client size，禁止缩放。
- desktop scene 的 node 顺序即 z 栈：全屏 Pixels 底图先行，随后每个窗口先按其 frame clip 重绘桌面像素、
  再叠加其 foreign surface，overlay clip（taskbar/菜单）居末。同一桌面 buffer 可在多个 Pixels node 按
  clip 复用；每个窗口的 chrome 与 content 必须原子叠放，任何窗口内容不得覆盖其他窗口的 chrome。
- app `SURFACE_COMMIT` 与 desktop `SCENE_COMMIT` 分别有 monotonic revision。frame latch 后到达的提交进入
  下一帧；每连接最多 64 KiB nonblocking outbound queue。可合并 event 覆盖旧值，不可丢事件无法入队
  时断开连接；禁止 compositor writer thread。
- client 提交后不得同步等待 `PRESENTED`；`ACCEPTED` 只确认 compositor 原子接纳 revision，
  `PRESENTED` 只确认 page-flip completion，buffer 只能由 `BUFFER_RELEASE` 重新变为 writable。
  双 buffer 都在途时 client 必须保留 latest-only dirty state，禁止排队栅格化旧 snapshot。
- buffer allocation 只经 compositor：每连接最多四个、session 最多八个 full-frame equivalent，按
  `pitch * height` 计费，scanout 不计入。allocation failure 明确返回，不得抢占别的连接、降低尺寸或
  让 client 自行 CREATE_DUMB。DESTROY 只由 compositor 执行。
- resize/maximize 使用 `CONFIGURE(serial)`；对应 app commit 进入 pending slot，直到 desktop scene 引用
  `CONFIGURE_READY(serial)` 才在同一 latch 切换 buffer 与 geometry。旧 pair 在 presentation 后释放。
  仅 foreign adoption/geometry 改变时 desktop scene 必须复用当前 pinned、只读像素 buffer，不得因此
  触发 React desktop 重栅格化。move 只允许由已投递 pointer-down serial 发起；compositor 对完整
  `windowGroup` 应用 bounded temporary transform。desktop 必须随授权提供一个排除该 group 的只读
  underlay buffer；compositor 用它恢复旧位置，只刷新旧/新 bounds damage，并在 pointer-up 返回最终
  logical position。最终 canonical scene 呈现后清除 grab 并 release underlay；期间到达的新 scene
  必须继承 transform，禁止跳回旧位置或保留 canonical 残影。
- scene input region 是 compositor routing 的唯一依据，pixel alpha 不参与 hit-test。每 node 最多 64、
  整份 scene 最多 256 个 input rectangle；超限拒绝，不得扩大到 bounds。app surface 默认使用完整 client rect。
- pointer motion 对同一 target latest-only，每帧最多一次；离散事件前必须先 flush preceding motion。
  button/key/wheel/focus 不可合并。capture 只能消费同一次 pointer-down 的 input serial，并在 up、unmount、
  focus loss 或 disconnect 时由 compositor exactly-once reset。
- cursor shape wire value 固定为 arrow、pointer、NS、EW、NESW 与 NWSE 六种；LiteUI 从标准 CSS
  `cursor` 值归一化，compositor 独占 checked 预乘 RGBA cursor asset（48×48 物理像素，`.lc2`）与
  hotspot。未知值必须回落 arrow，不接受应用 URL、位图或 theme asset。
- compositor 独占 pointer-focus surface 并据此仲裁 cursor 归属：`SetCursorShape` 仅在请求连接身份
  与当前 pointer-focus surface 一致时生效（surface_id 与连接不符视为协议错误）。pointer focus 切换
  或消失（含 capture 释放、app 断开、epoch reset）时先回落 arrow，新 target 在其首个 Motion 上报当前
  CSS cursor。LiteUI 不缓存全局 cursor owner；scanout 是形状去重与实际绘制的唯一 owner。
- compositor 必须把 evdev wheel detent 转为有符号 logical CSS pixel delta。LiteUI 先投递同值 wheel
  listener，再执行 scroll default action；`overflow: hidden/clip` 只裁剪且不响应 wheel，
  `overflow: auto` 仅在实际 overflow 时显示 scrollbar，`overflow: scroll` 始终显示。短内容的 offset
  必须为零；content/viewport 缩小时必须 clamp；嵌套容器只把本层未消费 delta 传播给 ancestor。
- global accelerator table 由 desktop 原子提交，compositor 只匹配固定 physical chord 并把完整 down/up
  sequence 路由 desktop。窗口 policy 与 shortcut action 不得进入 compositor。
- clipboard 只保存 session 内不超过 1 MiB 的 UTF-8 text，desktop 是内容 owner，compositor 只按
  connection routing read/write。无 image/file/HTML/primary selection。
- QuickJS 每个 host→JS turn 使用固定 interrupt-check budget；Promise jobs 与 microtask 共用该预算。
  desktop heap 32 MiB、app heap 16 MiB、VM stack 512 KiB。超限是 fatal；native host call 必须非阻塞。
- 同一 JS turn 内同步 React mutation、job drain 后最多产生一个 revision；rAF callbacks 共用一个 turn，
  不同离散 input 不跨 turn 合并。snapshot arena 不可用时只记录 dirty，归还后从最新 host tree 生成。
- app entry 必须 default export 一个 component。target loader 仅接受固定 React/LiteUI system module；
  `lite:apps`、`lite:desktop` 与 `lite:audio-system` 必须拒绝普通 app session。应用可通过标准
  `<audio>` 与 `lite:fs` 的 filesystem-backed `File` 播放；native plugin、dlopen、应用自建 worker
  与 Node API 不存在。
- terminal helper stdin/stdout 使用长度前缀 binary protocol，stderr 只诊断。screen update 按完整脏行，
  并携带 DECSCUSR 的 block/underline/bar 与 blink 状态；最多一个 update 在途，ACK 前变更合并；
  terminfo 同时发布通用 `Ss`/`Se` 和 Vim 外部 terminfo loader 使用的 `SI`/`EI`；resize 发送完整
  grid。helper argv 必须在 `--` 后显式给出，不提供默认 shell或 command-string parser。

## Failure and cleanup

- desktop disconnect 是 session epoch terminal transition：compositor 关闭全部 app socket、撤销 capture、
  release/destroy 全部 client buffer 并保留 DRM owner；app 观察 EOF 后退出，init 重启 desktop。
- ordinary app uncaught exception、OOM、budget exhaustion、invalid dynamic style/asset 或 display protocol
  error 只终止该 app。desktop 的同类错误终止 epoch。LiteUI 只写 stderr，不显示 error page 或恢复 UI。
- close request 同步 unmount 唯一 React root、关闭 helper/fd、断开 display 并退出；应用不可 veto，也没有
  before-unload hook。PTY child exit 使 terminal-session 同码退出，React terminal 随后退出。
- compositor 必须在 connection teardown 沿唯一 owner path 撤销 pending configure/commit、scene
  references、clipboard request、accelerator sequence、pointer/key state 与所有 GEM mapping/handle。
  partial decode、allocation 或 SCM_RIGHTS failure 不得发布 resource identity。
- boot scene 由 compositor 在取得 DRM 后立即显示并以 30 Hz 运行 indeterminate progress；没有固定
  最短时长。identity 资产只保存按最终物理像素生成的紧凑 logo/title XRGB 图层，compositor 不缩放，
  两层与进度条共享屏幕水平中轴。仅 desktop 首个完整 scene 成功 latch 后切换并永久释放 boot
  timer/buffer。desktop 失败时保持 boot scene并由 init 重启，不恢复独立 splash 进程。
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
- pointer/cursor poll、move damage accumulation 与 DIRTYFB clip staging 使用固定容量栈状态；持续拖动
  不得为 wake descriptor、damage 或 clip 创建临时 heap collection。
- 视觉还原不属于自动 gate，不生成 preview screenshot 或 Golden；真实启动后的外观由人工裁决。
