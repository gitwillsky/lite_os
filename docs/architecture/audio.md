# 音频当前架构

## 生产链路

LiteOS 只有一条音频播放路径：

`HTMLMediaElement` → LiteUI process audio worker → `audio-proto` shared PCM ring →
`audio-service` system mixer → Linux ALSA PCM OFD → `kernel::audio` →
VirtIO Sound device ID 25。

应用用 `lite:fs.open()` 取得 filesystem-backed `File`，再以
`URL.createObjectURL()` 产生 `blob:` source；app-relative resource 使用同一媒体状态机。
`http:`、`https:`、`data:`、`file:`、Media Source、MediaStream、capture 和 Web Audio
不属于当前能力。不存在按 path 播放的 Native audio API。

## 进程与数据面

- init 监督唯一 `/bin/audio-service`。服务不创建窗口；compositor 和 LiteUI 不负责拉起或恢复它。
- 每个 LiteUI process 只有一个 audio worker 和一条 AF_UNIX control connection；连接内最多复用
  8 条 logical stream，整个 session 最多 32 条。
- control connection 只传 v1 lifecycle、gain、generation、progress 和 typed error；每条 stream
  创建成功时只经一次 `SCM_RIGHTS` 发布固定 65,600-byte memfd（64 KiB PCM + 64-byte header）。
- 每条 memfd 是 8192-frame SPSC ring。worker 单写，mixer 单读；PCM 不经过 socket。
- service control thread 独占连接、配额、fd publication 和设置持久化；mixer thread 独占 ALSA、
  stream 消费、mix、limiter 与 progress。双向 control handoff 使用预分配有界 SPSC queue。

## PCM、调度与质量

worker 负责 probe、metadata、decode、trim、seek、channel conversion 与必要的带限重采样。进入共享
ring 后的唯一格式是 48 kHz、stereo、interleaved IEEE-754 `f32`。worker render block 固定 128
frames；mixer/ALSA period 固定 256 frames，device buffer 固定 4 periods。

mixer 只执行 per-stream linear gain、float accumulation、master gain 和最终 overload protection。
sum peak 不超过 1.0 时 limiter 完全透明；超过时使用 128-frame lookahead、无 makeup gain 的
brick-wall limiter，并以 50 ms release 恢复。steady decode/resample/mix 不分配，idle 不使用 timer
轮询。

## Web media 状态

LiteUI 实现冻结的 HTMLMediaElement playback surface。首次有声 `play()` 必须来自当前 app 的真实
pointer/key activation；成功后授权持续到该 app process 退出。muted media 可在授权前开始，但变为
audible 时重新检查。

seek 是 generation transaction：先发布 `seeking`，等待 mixer flush/ack 旧 generation，再由 decoder
定位到不晚于目标的合法 frame、精确 discard 到目标，然后发布新 generation 与 `seeked`。旧 completion
不能推进新 source。服务断开使旧 stream 永久失效，pending `play()` 返回 `AbortError`；元素停在最后
confirmed media time，不自动重连或恢复声音。

## 设备与目标

AArch64/HVF 是完整 runtime 与性能 owner。QEMU `virt` 暴露 modern VirtIO-MMIO
`virtio-sound-device,streams=1`：交互运行使用 CoreAudio，自动门禁使用 WAV/none backend。
RISC-V 复用同一 kernel/userspace source 并承担 compile、architecture、static 和 artifact gate，
不发布声音 runtime 或延迟通过声明。
