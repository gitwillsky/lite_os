# 音频契约

## Owner 与依赖

| Owner | 唯一职责 |
|---|---|
| `kernel::drivers::virtio_sound` | VirtIO Sound queue、DMA、command/completion、IRQ edge 与 reset |
| `kernel::audio` | PCM device、ALSA playback state、position、poll/xrun 与 per-OFD state |
| `platform::qemu_virt` | device ID 25 discovery、IRQ route 与 adapter 装配 |
| `fs` | `/dev/snd/pcmC0D0p` pathname 和 character OFD publication |
| `syscall` | Linux ALSA ioctl codec、user-copy、mmap/poll dispatch 与 errno |
| `user/linux-uapi` | ALSA、memfd、mmap 与 SCM_RIGHTS 的 typed wrapper |
| `user/audio-proto` | AF_UNIX v1 wire、stream identity 与 memfd SPSC ring layout |
| `user/audio-service` | device clock、stream registry、quota、mixer、limiter 与 master state |
| `user/lite-ui/audio` | 每进程 worker、File/media source、decoder/resampler、seek 与 Web state projection |
| `ui/runtime` | `HTMLMediaElement` public instance、Promise/event projection 与 UA controls |

`compositor` 不得依赖 audio crate、protocol 或 service。Web app 不得直接消费 `audio-proto`；
kernel ALSA owner 不得读取 VirtIO private queue state；VirtIO adapter 不得编解码 Linux ioctl。

## 固定接口

- kernel 只发布 `/dev/snd/pcmC0D0p` 的已登记 Linux ALSA playback 子集；未登记 request 返回精确 errno，
  不允许 success stub。
- userspace protocol 只有 v1，frame 不超过 4096 bytes。每个连接的 `u64` stream ID 单调且不复用；
  `CREATE_STREAM` 是唯一携带 fd 的 frame。
- ring 固定 8192 frames、48 kHz stereo interleaved `f32`，logical mapping 为 65,600 bytes；
  producer/consumer index 与 generation
  使用 acquire/release publication。任何越界 index、非法 generation 或 frame/fd 数量损坏都撤销
  当前 stream；协议身份损坏撤销整个连接。
- app quota 8、session quota 32 必须在 memfd publication 前原子预留；所有失败路径回滚两级 reservation。
- master state 只由 service 修改。普通 app 只能修改 element-local `volume`/`muted`；desktop-only
  `lite:audio-system` 只能取得 snapshot 和请求更新，不能成为第二 owner。

## Cleanup

- connection EOF 原子撤销该 process 的全部 stream、mapping、fd 与 app/session quota。
- source replacement、空 `src`/`load()`、元素 unmount 和 app exit 沿同一个 close owner 回收
  File、decoder、ring 与 stream。`URL.revokeObjectURL()` 只撤销未来解析；已加载 source 保持其
  File owner 到上述 media lifecycle close，禁止 revoke 反向中断正在使用的媒体。
- driver reset 先阻止新 submission，再完成或失败全部 outstanding command，最后释放 DMA；不得把旧
  used descriptor 归入新 generation。
- service orderly exit 先停止 ALSA device，再撤销 stream；crash 后旧 stream 不 reconnect。
- master 设置以 500 ms coalesce 后的 temp→fsync→rename 提交；损坏文件保留诊断并使用 75%、
  unmuted 安全默认，不能覆盖损坏现场。

## 实时与性能

- mixer steady period 不允许 lock、allocation、filesystem、socket framing 或等待 control thread。
- worker block 128 frames，mixer period 256 frames，device buffer 1024 frames；steady device wake
  不超过每秒 188 次。
- `play()` 到首次 device submission p95 不超过 50 ms，steady guest pipeline 不超过 25 ms，
  mix period p99 不超过 2.67 ms。
- idle service、worker 和 device 链路不允许 periodic wake；目标 workload 必须
  xrun=0、steady allocation=0。

## Web surface

公开 surface 只包含已冻结 ADR 中的 HTMLMediaElement playback 方法、属性、状态和事件。
`playbackRate` 仅接受 `1`；rate/track/EME/remote/capture/MSE/Web Audio 不得出现 silent-success
占位。首次有声播放的 user-activation grant 由 LiteUI input/media owner 持有，service 不复制。
`waiting` 属于冻结的事件类型，但固定本地文件预填充链路没有可观测 starvation transition，
正常路径不产生该事件；不得为虚构 transition 扩展协议。
