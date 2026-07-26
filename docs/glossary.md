# LiteOS 术语表

| 术语 | 定义 |
|---|---|
| Audio output | 从 Web runtime 产生音频帧，经系统音频服务与 kernel 设备 seam 播放到 host 的单向能力。 |
| Audio capture | 从 microphone 等输入设备采集音频帧并交给 Web runtime 的能力；不属于首个声音里程碑。 |
| Media element | LiteUI 对标准 `<audio>` / `HTMLMediaElement` 播放状态、控制方法和事件的实现。 |
| Web Audio | 以 `AudioContext` 和音频节点图提供实时合成、处理与精确调度的 Web API；不属于首个声音里程碑。 |
| Native file capability | 桌面应用用于发现、授权和打开本地文件的 `lite:fs` 能力；不拥有媒体解码或播放状态。 |
| Filesystem-backed File | 由 Native 文件能力授权并惰性读取的标准 `File` 对象；其内容不整体进入 QuickJS heap。 |
| Logical playback stream | 一个 media element 提交给系统 mixer 的独立播放序列；不直接拥有物理 PCM 设备。 |
| System audio service | 独占物理 PCM output、设备时钟、混音、格式归一化与 underrun 恢复的系统进程。 |
| ALSA PCM playback | LiteOS kernel 向 Native userspace 发布的 Linux-compatible PCM 输出 ABI；不暴露 VirtIO adapter。 |
| VirtIO Sound adapter | kernel 内独占 VirtIO Sound queues、DMA、command lifecycle、completion 与 reset 的具体设备实现。 |
| Audio worker | 每个 LiteUI 进程中唯一拥有媒体读取、解码、seek、PCM 排队和音频服务 IPC 的非 UI 线程。 |
| Transparent playback | 除必要格式转换、用户 gain 与 overload 保护外不主动改变波形的共享播放目标；不等同 bit-perfect。 |
| PCM normal form | audio worker、system mixer 与设备边界共同使用的 48 kHz stereo interleaved `f32` 格式。 |
| Codec baseline | media element 通过 Symphonia 0.6.0 可以真实声明并完整解码的全部稳定音频格式集合。 |
| Playback grant | 一次真实用户激活授予当前 app process 的有声播放许可；进程退出即撤销。 |
| PCM ring | 每条逻辑 stream 由 memfd 提供、audio worker 单写且 system mixer 单读的固定容量共享环。 |
| Render block | audio worker 产生的 128-frame PCM 工作单元；mixer 每个 256-frame device period 消费两个。 |
| Confirmed media time | system mixer 已确认消费的最后 PCM frame 所对应时间；服务失败时 `currentTime` 回落到该位置。 |
| Master volume | system audio service 在最终 mix output 唯一应用的系统级 gain/mute；不同于 element volume。 |
| Linear element gain | `HTMLMediaElement.volume` 的标准 `0..1` amplitude multiplier；不同于 master slider 感知曲线。 |
| Overload limiter | 仅在混合峰值将超过 1.0 时介入的 128-frame lookahead 保护；不是常驻 compressor。 |
| Live playback stream | 已申请 decoder/ring/mixer identity 的 media element stream；paused 时仍属于 live。 |
| WAV runtime gate | 用 QEMU WAV backend 记录全链路输出，再由 host 分析内容与 guest marker 的自动门禁。 |
| Audio domain | 从 VirtIO adapter、kernel ALSA PCM、system mixer 到 LiteUI media adapter 的独立 owner 链。 |
| Audio control connection | 每个 LiteUI audio worker 用于管理最多 8 条 stream 的唯一 AF_UNIX connection。 |
| Music Player | 只组合 `lite:fs`、filesystem-backed `File` 与标准 media element 的 production 验收应用。 |
