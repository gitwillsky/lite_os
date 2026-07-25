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
- global accelerator table 由 desktop 原子提交，compositor 只匹配固定 physical chord 并把完整 down/up
  sequence 路由 desktop。窗口 policy 与 shortcut action 不得进入 compositor。
- clipboard 只保存 session 内不超过 1 MiB 的 UTF-8 text，desktop 是内容 owner，compositor 只按
  connection routing read/write。无 image/file/HTML/primary selection。
- QuickJS 每个 host→JS turn 使用固定 interrupt-check budget；Promise jobs 与 microtask 共用该预算。
  desktop heap 32 MiB、app heap 16 MiB、VM stack 512 KiB。超限是 fatal；native host call 必须非阻塞。
- 同一 JS turn 内同步 React mutation、job drain 后最多产生一个 revision；rAF callbacks 共用一个 turn，
  不同离散 input 不跨 turn 合并。snapshot arena 不可用时只记录 dirty，归还后从最新 host tree 生成。
- app entry 必须 default export 一个 component。target loader 仅接受固定 React/LiteUI system module；
  `lite:apps` 与 `lite:desktop` 必须拒绝普通 app session。native plugin、dlopen、worker 与 Node API 不存在。
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

- first-class gate 是 AArch64+HVF、3008x1692、512 MiB、60 Hz，场景为 desktop、terminal 与第二窗口。
  window drag、菜单、scroll、terminal output、text input 与 background timer 的 frame p95 不超过
  16.67 ms、p99 不超过 33.3 ms，input-to-visible p95 不超过 33.3 ms。
  该 frame 预算由 `scripts/verify_frame_timing.py` 经 compositor 的 guest-vblank `compositor: frame-stats`
  marker 强制（见 build-and-verify 性能测试段），不再仅由契约文本维护。
- idle 不允许 render/commit/periodic wake；steady renderer/compositor frame 不允许 allocation。
  compositor+desktop+两个 app 总 RSS 不超过 256 MiB。RISC-V TCG 只承担正确性，不承担 60 Hz gate。
- pointer/cursor poll、move damage accumulation 与 DIRTYFB clip staging 使用固定容量栈状态；持续拖动
  不得为 wake descriptor、damage 或 clip 创建临时 heap collection。
- 视觉还原不属于自动 gate，不生成 preview screenshot 或 Golden；真实启动后的外观由人工裁决。
