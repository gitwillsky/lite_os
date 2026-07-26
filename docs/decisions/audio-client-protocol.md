# ADR: 每个 LiteUI 进程复用一条 audio control connection

- 状态：已接受
- 日期：2026-07-26

## 背景

per-element socket 会放大 fd、poll 和 teardown；全 session 共用一个 client connection 又不能把
quota、failure 和 app exit 绑定到准确进程。PipeWire/CRAS 类设计使用 client control connection
管理多个 stream object，并为每条 stream 建独立共享数据 buffer。

## 决策

- 每个 LiteUI process 的唯一 audio worker 建立一条 AF_UNIX control connection，复用最多 8 条 stream。
- protocol version 精确为 v1；版本不匹配直接拒绝，不做 capability negotiation 或兼容 message。
- control frame 最大 4 KiB；PCM 永不进入 control payload。
- service 为每条 stream 分配 connection-scoped、该连接生命周期内不复用的 64-bit ID。
- `CREATE_STREAM` success 是该 stream 唯一一次 `SCM_RIGHTS` memfd publication。
- start/pause/flush/gain/close 与 progress/error 以 stream ID + generation 路由。
- connection EOF 原子撤销该 process 全部 stream、memfd mapping 与 app/session quota。

## 结果

- app process identity、per-app quota 和 connection cleanup 共享一个 owner。
- 每条 stream 的 PCM ring、generation、media state 仍相互隔离；一条 stream error 不关闭兄弟 stream，
  protocol/frame/identity corruption 才关闭整个 connection。
- future sandbox 可以在连接建立前增加 broker/permission owner，不需要改变 mixer 或 ring wire。
