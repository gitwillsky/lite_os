# Phase 48：EPOLLEXCLUSIVE source wake-one

本阶段固定对照 Linux v7.1 commit `8cd9520d35a6c38db6567e97dd93b1f11f185dc6` 的 `fs/eventpoll.c`、`include/uapi/linux/eventpoll.h` 与 wait-queue exclusive callback 语义，只扩展既有 IndexedWaitQueue，不建立 epoll 私有 waiter registry。

## ABI validation

- `EPOLLEXCLUSIVE` 只允许 `EPOLL_CTL_ADD`，并且只能组合 `EPOLLIN/EPOLLOUT/EPOLLERR/EPOLLHUP/EPOLLWAKEUP/EPOLLET`。
- exclusive interest 禁止以 epoll fd 为 target；已经 exclusive ADD 的 `(epfd,fd,OFD)` 禁止 MOD，DEL 保持正常。
- `EPOLLERR|EPOLLHUP` 与 Linux 一样在内部自动加入 interest；无 suspend owner 时 `EPOLLWAKEUP` 继续按非特权语义清除。

## 唯一 wait owner

- `PollWaitKey` 保存 source identity、direction、requested events、exclusive mode 与 epoll instance wake-group。一个 Poll membership 在同一 source 上出现重复 key 时合并 event mask；只要存在普通 key，普通模式优先，避免同一 task 同时占用 wake-all 与 wake-one 两条注册。
- IndexedWaitQueue 的 console/Pipe index 将 exclusive mode 纳入 key，但 membership、deadline、signal cancellation 和 task scheduling state 仍保持单一 owner。
- source wake 先按实际 ready mask 过滤 callback：每个匹配的普通 epoll instance 只选择一个 epoll_wait thread，同时唤醒全部匹配的 ppoll/direct waiter；随后按 wait-id 选择一个匹配 exclusive epoll instance。被选 waiter 重新注册后获得更大的 wait-id。
- Pipe read source 投影 `IN/HUP`，write source 投影 `OUT/ERR`；这避免只订阅 `EPOLLIN` 的 exclusive socket 被无关 `EPOLLOUT` wake 错误消费 quota。

因此同一 target 同时被普通和 exclusive epoll 监控时，单次 source event 会在每个匹配普通 epoll instance 上唤醒一个 thread，并在全部 exclusive epoll instance 中只唤醒一个 thread；只有 exclusive 时执行真正的 wake-one。

## 状态机验收

- 两个 exclusive epoll instance 阻塞在同一 Pipe read source：一次 `IN` source wake 只从 exclusive index 移除一个 membership，另一个保持 Blocking。
- 两个普通 instance、两个 exclusive instance 和一个 ppoll waiter 共存：一次匹配 wake 消费 ppoll、每个普通 instance 的一个 thread，以及全部 exclusive 中的一个 thread。
- `EPOLLIN|EPOLLEXCLUSIVE` socket 同时注册 receive/read 与 transmit/write Pipe：单纯 `OUT` ready 不匹配其 event filter，不会消费 exclusive quota；peer close 的 `ERR/HUP` 仍无条件匹配。
- signal/timeout 与 source wake 竞争：三条路径都先在同一 IndexedWaitQueue lock 下 `remove(id)`，只有取得 membership 的路径能提交 SchedulingState wake result。
- exclusive ADD 后，无论 MOD 是否再次携带 `EPOLLEXCLUSIVE` 都返回 `EINVAL`；DEL 正常移除全部 source indexes。
