# ADR: 声音能力以可分析输出和真实全链路 gate 完成

- 状态：已接受
- 日期：2026-07-26

## 背景

QEMU 枚举出声卡、guest 写入 PCM 或人工听到声音都不能分别证明 Web API、codec、mixer、时序和
cleanup 正确。把尚不存在的 runtime test 写入权威验证文档会造成比缺失更危险的虚报。

## 决策

声音完成必须同时具备并通过：

- host/unit：VirtIO command/completion/reset、ALSA ioctl/state、SPSC ring/generation、quota/autoplay、
  media event ordering、全部 codec fixtures 的 frequency/duration/channel/RMS/trim/order 及
  valid/truncated/corrupt 边界、resampler、mixer 与 limiter；
- AArch64 QEMU WAV backend：boot 不产生音频，测试应用经真实 UI activation 启动；
- target 全链路依次播放每个受支持 container/codec，并验证 seek、pause/resume、loop、volume/mute；
- 同一 production run 的整体 WAV 裁决 frequency、duration、channel identity、RMS 与 peak；8 个
  production Music Player stream identity 必须在同一窗口各自产生 consumption progress，并对该
  窗口的 limiter activation/reduction 与 service 性能指标作裁决。连续混音 WAV 不虚报为具备
  per-stream segment boundary，单 codec trim/order 的 owner 是上述逐 fixture host test；
- guest marker：xrun=0、steady allocation=0、idle periodic wake=0；
- mix period p99 ≤ 2.67 ms，即不超过 5.33 ms device period 的一半；
- host 对 QEMU 生成 WAV 作确定性分析；CoreAudio 只作交互人工验收。

只有 gate 已实现、实际通过并接入 `make verify` 后，才能在 `build-and-verify.md` 声明对应覆盖。

## 结果

- 每个已公布 codec 至少有一个合法、truncated 和 corrupt fixture；不以 host-only probe 外推 target 支持。
- WAV analyzer 与 guest markers 必须裁决相同 run identity，不能组合不同运行的局部成功。
- 不能放宽 buffer、period、deadline、stream count 或 marker 集合来接受实现错误。
- RISC-V 首期只承担已接受的 compile/static/artifact 范围，不虚报声音 runtime。
