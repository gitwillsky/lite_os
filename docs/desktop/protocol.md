# LiteUI application protocol

## HostOps boundary

Solid universal renderer 不接触 DOM。首期 JS adapter 只暴露 node creation/removal、ordered child
mutation、typed property/style update、text update、event subscription、window request 与 animation
declaration。任何 path、socket、DRM、input 或 arbitrary native call 都不属于 UI protocol。

## Connection identity and UI transaction

连接身份由 session 创建的 uid、`SO_PEERCRED` 与固定 slot 一次绑定：100 是 System Shell，101 是
terminal service，102 是普通应用。每 slot 最多一个连接并独立拥有 generation/sequence；wire 不复制
可伪造的 application id。System Shell 与普通应用只发送 `LUI1`，terminal service 只发送 `LUG1`。

`LUI1` v1 header 固定为 40 bytes little-endian：magic、version/header bytes、session epoch、sequence、
operation count、payload bytes、flags 与 reserved；随后是固定 40-byte operation 数组。payload 上限
256 KiB、operation 上限 256；flags/reserved 必须为零，payload 长度必须精确等于 operation count × 40。

compositor 按以下顺序处理：

1. 完整 copy envelope 到该 client 的 bounded staging storage；短读只推进 codec，不修改 scene。
2. 校验 header、epoch、sequence、长度、node generation、tree cycle、typed property 与 quota delta。
3. 在不可见 transaction projection 上应用 mutation；任何错误整体丢弃。
4. 原子发布该 slot 的新 subtree generation，计算 layout/draw/damage，并在下一 frame commit。
   client EOF、`POLLHUP` 或非法 frame 都关闭连接，并无分配复位该 slot 的 node generation/sequence。

首期选择 socket copy 是为了消除 shared-memory TOCTOU。未来 transport 优化必须复用完全相同的
envelope/validation/commit contract，并由 profiling 证明必要性。

## Operation codec v1

每个 operation 固定 40 bytes，未登记的 flag/reserved byte 必须为零：

- `1 Create` / `2 SetStyle`：node identity 位于 byte 2..6，parent identity 位于 6..10；
  byte 12..28 是 parent-relative signed pixel rect，28..36 是 background/border RGB，36 是
  border width，37 是互斥校验后的 anchor bits，38 是有界 semantic role，39 保留为零。
  role v1 精确为 normal/window/titlebar/close/minimize/maximize/restore/action/text-grid（0..=8）。
  compositor 直接处理窗口 role；action 只回送 node identity；text-grid 只定位 compositor-owned
  terminal resource。应用不能上传任意 action code。`SetStyle` 的 parent 必须为零 identity。
- `3 Remove`：除 opcode 与 node identity 外全部为零。删除 node 同时删除全部 descendants。
- `4 SetText`：byte 1 只允许 bold bit；byte 6 是 1..24 的 UTF-8 byte length，8..12 是 RGB，
  12..36 是 NUL-free inline UTF-8，尾部 padding 必须为零。文本 run 与 node 同属 staging
  transaction；compositor 不保存 JS string、裸 pointer 或帧内 heap allocation。

node/style 与 text 分成两个 operation，使常规几何热路径不复制 24-byte text，同时保持 codec
固定宽度、无不可信 offset。Rust projection 把每个有文本的 visible node 最多展开为一个 rectangle
和一个 text primitive，因此 DrawList 在 admission 时按 `2 × node quota` 一次性预留。

## TextGrid publication

`LUG1` 与 `LUI1` 共用 40-byte prefix 中的 version、header bytes、epoch 与 sequence；byte 24..28
是 columns/rows，28..32 是 payload bytes，32..36 是 cursor column/row，36..38 是 reverse 与
blink-visible flags，38..40 保留为零。cursor 两坐标必须同时为 `u16::MAX` 或同时位于网格内。

payload 是 `rows × columns` 个 16-byte cell，按 row-major 排列：Unicode scalar、foreground RGB、
background RGB、attributes 与 reserved。attributes 只允许 bold/dim/underline/inverse/hidden/blink；
reserved 必须为零，总 cell 数不得超过 16,384（256 KiB / 16-byte cell），frame payload 仍不得超过 256 KiB。compositor 完整
验证并复制到不可见 TextGrid state 后只做一次 swap；失败不推进 sequence、不改变 active grid。
terminal backpressure 时保留 ANSI model 这一份最新事实，禁止排队多个历史 grid snapshot。

## Compositor events

compositor→client 的 `LUE1` frame 固定 24 bytes：magic、version、kind、8-byte payload 与单调
sequence。每 slot 使用固定 64-frame ring；输入发布不分配。kind 1 是 action click（local node
index/generation），kind 2 是 Linux evdev key code/value，kind 3 是 TextGrid columns/rows/pixel
width/pixel height，kind 5 是 X10/VT200 button/press/cell coordinates。kind 2/3/5 只发往 terminal
slot；TextGrid 未聚焦时键鼠不得进入 PTY。

keyboard ring 剩余不足一个完整 evdev batch 时 compositor 停止读取 keyboard OFD，让 evdev queue
保留 backpressure；`SYN_DROPPED` 以 key code `u16::MAX` 通知 terminal 清空 modifier snapshot。
configure 只按最新 geometry 去重并在 ring 腾出空间后重试，不能用丢事件或无限 queue 换取进度。

## Time and input

compositor 是 `FrameId` 与 monotonic UI time 的唯一 owner。Rust animation 使用固定逻辑 tick；
JS timer 只注册 deadline。输入先由 compositor hit-test，再返回 window/node identity、frame epoch
与语义 event。拖动/缩放属于 Rust window state machine，不能等待 JS event round trip。
