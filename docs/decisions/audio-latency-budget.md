# ADR: 音频 render quantum、buffer 与延迟预算

- 状态：已接受
- 日期：2026-07-26

## 背景

过小的 device period 增加 IRQ、syscall 和 mixer wakeup；过大的 buffer 会把实现错误隐藏成可听延迟。
Web Audio future integration 又需要稳定的小 render block。没有固定数值就无法验证“低延迟且 CPU 有界”。

## 决策

首期固定：

| 项目 | 数值 |
|---|---:|
| audio worker render block | 128 frames / 2.67 ms |
| mixer 与 ALSA device period | 256 frames / 5.33 ms |
| device buffer | 4 periods / 1024 frames / 21.33 ms |
| 每 stream PCM ring | 8192 frames / 64 KiB PCM payload + 64-byte header |
| steady device wake 上界 | 188 次/秒（向上取整） |
| `play()` 到首次 device submission p95 | ≤ 50 ms |
| steady guest audio pipeline | ≤ 25 ms |

audio worker 可以批量填充多个 128-frame block；mixer 每个 period 一次消费两个 block。目标 workload
不允许 xrun，idle 时服务、worker 和设备链路均不得周期唤醒。

## 结果

- 未来 Web Audio 可以复用 128-frame render quantum，不改 PCM ring 格式。
- 不能提高 period、buffer 或 prefill 来让 xrun gate 通过；应修复 decode、wakeup 或 queue 根因。
- runtime gate 必须分别记录 submission/consumption、xrun 和 wakeup，host 扬声器是否可听不能替代 guest marker。
