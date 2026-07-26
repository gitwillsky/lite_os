# ADR: 在共享 mixer 下追求透明且 CPU 有界的播放

- 状态：已接受
- 日期：2026-07-26

## 背景

bit-perfect exclusive playback 与多应用混音、系统音量和通知并存相冲突。盲目使用 `f64`、
96/192 kHz 或每流动态处理会增加 CPU、内存带宽和 wakeup，却不能恢复有损或低采样率来源中不存在的信息。

## 决策

首期不提供 exclusive/bit-perfect bypass，只有系统 mixer 一条生产路径。在该路径内：

- decode 后使用至少 `f32` 精度，避免整数混音累加溢出和重复量化；
- 只在源格式与 mix format 不同的时候重采样；
- 重采样必须是有界、预计算系数的带限实现，不能使用线性插值或每帧分配；
- 不增加默认 compressor、音效或响度归一化；除用户设置的 gain 和最终 overload 保护外不改写波形；
- steady decode/resample/mix 不分配，CPU、wakeup、underrun 与输出正确性必须进入验证。

唯一 overload 保护是 system mixer 的 128-frame（2.67 ms）lookahead brick-wall limiter。sum peak
不超过 1.0 时它不改变 PCM；超过时立即降低 gain，并以固定 50 ms release 恢复。不得使用 makeup gain、
常驻 compressor 或 loudness normalization。limiter activation count 与最大 gain reduction 必须进入
诊断和 runtime gate，避免长期 limiter 掩盖上游 gain 错误。

## 结果

- 多应用可以同时发声，音乐播放仍以透明转换为目标。
- 不建立“高质量”和“低 CPU”两套 engine；同一实现由 source format fast path 与有界 resampler 覆盖。
- lookahead 增加的 2.67 ms 必须包含在既有 25 ms guest pipeline budget 内。
