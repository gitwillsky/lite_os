# ADR: 音频使用独立 kernel 与 userspace owner 链

- 状态：已接受
- 日期：2026-07-26

## 背景

声音横跨具体 VirtIO transport、Linux PCM ABI、系统混音和 Web media。把 ALSA 状态放进 adapter、
把 mixer 放进 compositor，或让 LiteUI 直接拥有设备，都会倒置依赖并复制生命周期。

## 决策

| Owner | 职责 |
|---|---|
| `kernel::drivers::virtio_sound` | VirtIO queues、DMA、command/completion 与 reset |
| `kernel::audio` | 通用 PCM、ALSA playback state、position、poll/xrun 与 OFD backend |
| `platform::qemu_virt` | device ID 25 discovery、IRQ 与 adapter 装配 |
| `fs` | 发布 `/dev/snd/pcmC0D0p` device node |
| `syscall` | 固定 Linux ioctl codec、user-copy 与 errno |
| `user/audio-service` | mixer、ALSA OFD、master volume 与 stream registry |
| `user/audio-proto` | AF_UNIX control wire 与 memfd ring layout |
| `user/linux-uapi` | ALSA、memfd、mmap、SCM_RIGHTS typed wrapper |
| `user/lite-runtime/audio` | 每进程 audio worker、media state、decoder/resampler 与 Web event projection |
| `ui/runtime` | `HTMLMediaElement` public instance 与 UA controls |

compositor 不依赖任何 audio module，也不启动、监督或恢复 audio service。

## 结果

- 新增独立 audio architecture/contract 文档，并在全局 dependency matrix 登记 `audio`，不能把事实复制到
  `devices-terminal` 或 `lite-runtime`。
- kernel concrete adapter 只实现通用 PCM device seam；ALSA UAPI 不读取 VirtIO private state。
- system service protocol 是 userspace seam，不进入 kernel ABI；Web app 不直接消费该协议。
