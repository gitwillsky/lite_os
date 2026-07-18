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

## Failure and cleanup

- DMA/storage 必须在 publication 前预留；queue ownership、fence 或 descriptor mapping 损坏时 fail-stop。
- DRM close/RMFB/disable、evdev revoke、PTY master close 与 session exit 必须沿唯一 owner seam 清理并在锁外发布 consequence。
