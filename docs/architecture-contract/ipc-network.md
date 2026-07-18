# IPC 与网络契约

## Owner

- `ipc::Pipe` 独占 byte ring、endpoint count、atomicity 与 readiness generation。
- `fs::Epoll` 独占 interest/ET/ONESHOT/nesting state；IndexedWaitQueue 独占实际 wait membership。
- AF_UNIX socket、rights graph、IPv4 NetworkStack、AF_PACKET registry 与 kobject registry 分别独占各自 namespace、queue 和 protocol state。

## Interface

- notification pipe 只承载 edge；所有 poll/epoll/blocking caller 必须在 wake 后复查 backend level readiness。
- syscall socket 层只处理 sockaddr/iovec/msghdr/cmsg/option codec、user-copy 与 errno；不得匹配或泄漏 concrete protocol adapter。
- protocol message limit 与 stream/atomic classification 由 `socket::message_limits` 唯一提供。
- smoltcp、VirtIO-net 与 Linux socket ABI 通过 network-device 和 socket façade 分隔，任何一层不得复制另一层状态。

## Failure and cleanup

- send/receive publication 在 payload、control、fd reservation 与 queue capacity 全部验证后提交；partial stream progress 与 atomic message 失败必须区分。
- hardirq 不分配；deferred 网络处理有 batch 上限。close/drop 不得在 registry/graph lock 内反向调用 socket state。
