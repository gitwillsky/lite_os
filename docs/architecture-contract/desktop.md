# Desktop runtime architecture contract

> 权威入口：[architecture-contract.md](../architecture-contract.md)
>
> 设计索引：[desktop/README.md](../desktop/README.md)；实施与退出条件：
> [phase-61-desktop.md](../phase-61-desktop.md)。本文定义 Phase 61 必须建立的稳定
> owner/interface，不把尚未完成的能力写入当前实现事实。

## 1. Process 与状态 owner

- `liteui-session` 是一个图形 session 的 generation、进程成员、应用身份、APK policy
  与 capability publication 的唯一 owner。init 只监督 session，不得同时 respawn compositor、
  Shell 或图形 terminal。compositor generation 变化必须使旧 window、node、resource capability
  一次性失效；禁止按进程复制 generation 并人工同步。
- `liteui-compositor` 是图形 session 内 DRM、evdev、hotplug、window tree、focus、pointer
  capture、frame clock、retained UI scene、font cache、damage 与 scanout 的唯一 owner。普通应用、
  System Shell 与 terminal service 禁止打开 DRM/evdev 或持有 display-session device capability。
- `liteui-host` 每进程只拥有一个应用的 QuickJS Runtime、Solid adapter、microtask queue、timer
  registration 与 IPC connection。`publisher.rs` 的 boxed `Publisher` 是 QuickJS callback opaque
  地址、单槽待发送 frame、发送 offset 与 transaction sequence 的唯一 owner；地址在 Runtime
  销毁前不移动，单槽未排空即同步拒绝下一次 commit。一个 Runtime 不得装载多个应用；QuickJS
  不提供 `std`、`os`、FFI、裸 path 或裸 socket API。
- `terminal-service` 唯一拥有图形 TerminalSession 的 PTY child、ANSI parser、screen model 与
  reflow；compositor 只拥有通用 `TextGrid` resource，System Shell 只拥有其窗口装饰与布局。
  UART ash 是独立 recovery console，不得成为第二个图形 display owner。

## 2. UI core 与 transaction

- `liteui-core` 是无 I/O、无时钟、无设备依赖的 Rust deep module；唯一拥有 node arena、typed
  style、layout projection、animation state、text shaping projection 与 backend-neutral DrawList。
  compositor 只能通过有界 transaction、frame tick、hit-test 与 draw façade 使用它，禁止读取或
  修改内部 collection。
- Phase 61 的 text projection 固定为每 node 一个最多 24-byte 的 inline UTF-8 run；compositor
  启动时一次校验 16×32 Medium/Bold A8 atlas，raster 只做 clipped alpha blend。每帧禁止字形
  查找表扩容、string allocation 或宿主字体 fallback；更复杂 shaping 必须以后端无关 resource
  取代该有界 seam，不能旁路建立第二套 text owner。
- Solid universal renderer 只生成 typed `ui.*` mutation。每次响应式 turn 最多发布一个有界
  binary transaction；compositor 必须先完整 copy 到自有 staging storage，再验证 version、epoch、
  sequence、node identity、tree invariant、property codec 与 resource quota。全部成功后才能原子
  commit；失败不得留下部分 tree mutation。
- 每应用 subtree 有固定 node、text byte、asset byte、animation、window 与 transaction budget。
  quota admission 必须发生在任何 scene mutation 前；禁止通过 allocator abort 实现应用 OOM。
- compositor 是唯一 frame clock owner。Rust animation 使用固定逻辑 tick；掉帧可推进多个 tick，
  但每个 display refresh 最多提交一次。idle 且无 timer/deadline 时所有 reactor 必须无限阻塞。
- 首个 renderer 是 deterministic tile software backend。DrawList 与 tile binning 不得依赖 DRM；
  pointer overlay、window exposure 与 animation 只标记精确 damage。后续 GPU backend 必须消费同一
  DrawList，禁止复制 layout/window policy。
- `liteui-compositor::server` 唯一拥有 `/run/liteui/compositor.sock`、三个固定 identity slot、每 slot
  的短读 staging、decoded mutation/grid staging、generation/sequence 与 64×24-byte outbound event ring。
  `SO_PEERCRED` 只把 uid 100/101/102 分别绑定为 System Shell、terminal service 与普通应用；同 identity
  的第二连接必须拒绝。UI slot 只接受 `LUI1`，terminal slot 只接受 `LUG1`；断线只撤销该 slot 的
  subtree 或 TextGrid，不得影响其他 client。reactor 只消费已验证 transaction 与 damage，不读取
  codec 内部状态。
- `liteui-core::TextGrid` 是 terminal cell publication 的唯一 owner：启动期一次预分配两份
  16,384-cell state，`LUG1` 完整校验 epoch/sequence/dimensions/cursor/scalar/attributes 后复制到不可见
  staging，再以单次 swap 发布。renderer 只读 active snapshot，禁止在 cell decode、blink、resize 或
  raster 热路径分配；terminal service 只保留 ANSI model 作为最新事实，socket backpressure 下不得
  累积历史 frame。

## 3. Window、Shell 与失败恢复

- Rust window state machine 唯一拥有 geometry、z-order、focus、capture、minimize、maximize、
  close 与 resize transaction。受信 System Shell 的 Solid subtree 只定义窗口装饰、taskbar、菜单
  与 theme；拖动/缩放热路径不得等待 QuickJS round trip。
- retained node 只携带固定 `NodeRole`，DrawList 保留 node/window identity；compositor 依据 role
  进行反向 painter-order hit-test，并把 compositor-owned geometry 投影到同一 subtree。
  titlebar/close/minimize/maximize/restore/window/TextGrid 只触发 Rust 内建语义；`Action` 只把命中的
  node identity 送回其所属 host，不能携带应用自定义 opcode。缺少该边界会让 JS round trip 重新
  进入 pointer 热路径或把任意 RPC 混入 scene graph。
- System Shell 是早启 QuickJS 应用。compositor 在 Shell 首个完整 root transaction 前只显示固定、
  无脚本的 recovery scene；首帧在 display frame boundary 原子替换，禁止 black-frame handoff 或
  hydration 双 owner。首期无登录，session 自动降权到固定非 root identity。
- Shell crash 只撤销其 system capability 并重启 Shell；普通应用与 compositor 保持运行。
  compositor crash 必须终止该 generation 的全部 host/service 后重建完整 session。连续基础设施
  failure 回到 UART recovery，不得循环自旋。

## 4. APK application profile

- Alpine APK 是唯一安装、签名、升级、依赖和数据库格式；禁止再嵌套 `.lpk`。LiteUI 应用安装在
  `/usr/lib/liteui/apps/<package-local-name>/`；manifest 内全局 application id 是签名内容，目录名
  不复制 identity policy。目录精确包含 `manifest.cbor`、`app.mjs`、`styles.bin` 与 manifest 登记的
  assets。
- `app.mjs` 是权威可移植程序。QuickJS bytecode 只是以 QuickJS build ID、LiteUI ABI、compiler
  options 与 APK content hash 为 key 的可删除 cache；engine 升级不得要求重建、重签或重装 APK，
  也不得保留旧 Runtime 兼容 bytecode。
- LiteUI APK 禁止 install/upgrade/deinstall script 与 trigger。manifest 只能声明 capability request；
  session policy 依据签名 identity 决定实际授权，应用不得自授权。
- host build 唯一负责 TypeScript/JSX transform、Solid bundle、CSS subset compilation 与 asset
  preparation。guest 不安装 Node/npm/TypeScript/bundler；开发与 release 必须产出相同 APK profile。

## 5. Resource 与 unsafe proof

- `liteui-compositor::allocator::ALLOCATOR` 是 compositor 进程唯一 Rust heap adapter：
  max-align allocation 转发给 musl `malloc`，over-aligned allocation 以满足 C11 size
  multiple 前置条件的 `aligned_alloc` 完成，统一由 musl `free` 回收。该 heap 只承载启动期
  固定容量 scene/DrawList 与后续有明确 quota 的 client state；预分配失败时 recovery scene
  不具备原子 publication 证明，进程必须 fail-stop，禁止建立第二 allocator 或退化场景。
- 每个 `liteui-host` 只拥有一个 QuickJS Runtime/Context；QuickJS heap 由 engine allocator 与
  `JS_SetMemoryLimit` 独立计量，Rust source/error scaffolding 由进程唯一 musl allocator 拥有，两者
  禁止混合，否则应用 quota 无法证明。interrupt handler 只读取 host 设置的 monotonic deadline，
  cold source compile 上限 2000 ms、compiled module evaluation/startup 上限 100 ms、单轮 job 上限
  4 ms；deadline 缺失会让 JS 永久占用 session reactor。
- 首期 System Shell QuickJS heap 上限 8 MiB，普通应用默认 4 MiB；UI scene state 不含 framebuffer
  的目标上限 16 MiB。双 1920x1080 XRGB framebuffer 约 16 MiB，必须作为独立 scanout budget
  报告，禁止混入 JS heap 或 scene quota。
- 新增 global、lock、Atomic、cache、unsafe 或 publication flag 时，必须在相邻文档注释登记唯一
  owner、同步/有效性证明及缺失时的具体 failure。跨进程协议不得把不可信 shared memory 解释为
  Rust reference；首期 transport 固定为有界 Unix socket copy transaction。
