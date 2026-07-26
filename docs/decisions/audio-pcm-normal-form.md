# ADR: 音频链路固定 48 kHz stereo float PCM

- 状态：已接受
- 日期：2026-07-26

## 背景

允许每条逻辑 stream 向 mixer 提交不同 rate、channel layout 和 integer/float format，会把格式分派、
重采样与量化扩散到 mixer 热路径。固定 `S16_LE` 又会让已经解码为 24-bit/float 的来源在进入设备前
发生不必要量化。盲目使用 96/192 kHz 则显著增加 CPU 与内存带宽，不能恢复源中不存在的信息。

## 决策

首期唯一 PCM 正规形是 48 kHz、stereo、interleaved IEEE-754 `f32`：

- audio worker 把 decoder 输出转换为该格式；
- 48 kHz stereo source 使用零重采样 fast path；
- 其他采样率使用预计算、有界、steady-state 无分配的高质量带限 resampler；
- mono 展开与多声道 downmix 只在 audio worker 执行；
- system mixer、ALSA PCM 与 VirtIO Sound first-class backend 都使用同一 48 kHz stereo float 格式。

设备若不公布 `FLOAT`、48 kHz 或双声道能力，first-class backend 初始化显式失败，不静默退回
`S16_LE` 或第二套 engine。

## 结果

- 常见 24-bit integer PCM 可以无再次量化地进入 float mixer。
- mixer 只执行 gain 与 float accumulation，不按 stream 选择 codec、rate 或 sample format。
- 96/192 kHz 和 surround 不属于首期；未来改变正规形必须重新评审 CPU、ring capacity 与所有 consumer。
