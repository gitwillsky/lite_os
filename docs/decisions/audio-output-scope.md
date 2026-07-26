# ADR: 首个声音里程碑只支持播放

- 状态：已接受
- 日期：2026-07-26

## 背景

LiteOS 当前没有音频设备 driver、PCM 用户态 ABI、音频服务或 Web 音频 API。声音采集还会引入
设备授权、隐私提示、输入选择、duplex 时钟与录制生命周期，不能作为播放路径的隐含分支。

## 决策

首个声音里程碑只实现 audio output，不实现 microphone capture、录音或 duplex stream。
播放链路必须作为完整桌面 Web runtime 能力设计，不能绑定单个应用。

## 结果

- 设备、kernel seam、用户态服务与 Web API 只为播放建立唯一生产路径。
- capture 所需的权限、隐私、输入设备和录音状态不进入首个里程碑。
- 未来引入 capture 时必须单独评审其 owner、权限模型和 Web 标准接口，不能复用输出状态冒充输入能力。
