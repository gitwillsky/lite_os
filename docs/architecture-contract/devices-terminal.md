# 设备与终端契约

## Owner

- concrete VirtIO adapter 独占 queue、DMA、descriptor、completion 与 reset state；`drivers` 只发布通用 device seam。
- `drm::DrmDevice`/`DrmFile` 独占 display/KMS/GEM/framebuffer/master/event state；`input::EvdevDevice`/`InputFile` 独占 input/client state。
- `fs::pty` 独占 PTY registry/pair；Terminal 独占 session/foreground/termios/winsize；`console-session` 独占 ANSI parser 与 renderer state。

## Interface

- platform 是 concrete adapter 的唯一装配者；driver、DRM、input、filesystem 与 syscall 不得依赖 QEMU machine types。
- DRM/evdev syscall 只编码固定 Linux UAPI。devfs 只发布 object identity，不拥有 device state。
- display completion、input packet 与 PTY byte readiness 统一投递 semantic event；hardirq 不执行 renderer、filesystem 或 task logic。
- terminal userspace 只能使用标准 PTY、termios、signal、ANSI/ECMA-48；禁止私有 console syscall/protocol。
- Console write 是同步且非阻塞的 output drain seam；Terminal state lock 必须覆盖普通 output 与 input
  echo 的完整 Console write，TCSETSW 取得该锁后才应用设置。TCSETSF 还必须在 Terminal→Console
  唯一 lock order 下同时丢弃 raw adapter input、cooked queue、partial line 与 EOF；未来 adapter 若在
  `Console::write` 内阻塞等待将破坏该临界区契约。

## Failure and cleanup

- PTY master→slave line discipline 与 UART console 共用 256-byte input batch；UART 批末 raw backlog
  必须重新发布 deferred work，不能依赖用户可见 readiness 继续 drain。
- PTY master syscall write 的 user-copy chunk 同样限制为 256 bytes，并在返回前同步 drain 完整
  chunk；因此用户可见 `POLLIN` 只投影 cooked input/canonical EOF，未成行 raw bytes 只供内部
  `wait_ready(raw || cooked)` 封闭进度竞态。其他 character backend 保持 512-byte chunk。
- PTY registry 通过 composition root 保存不可变 input-signal callback；PTY master drain 生成的 ISIG bitset 必须由 task owner 路由到当时的 foreground process group，filesystem 不得反向依赖 task graph。
- DMA/storage 必须在 publication 前预留；queue ownership、fence 或 descriptor mapping 损坏时 fail-stop。
- DRM CREATE_DUMB/ADDFB 必须先预留 backing、object node 与 identity；完整 ioctl copyout 成功后才
  无分配发布 handle/ID，copyout failure 必须回收资源并释放未发布的最新 identity。
- DRM close/RMFB/disable、evdev revoke、PTY master close 与 session exit 必须沿唯一 owner seam 清理并在锁外发布 consequence。
