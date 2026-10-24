# 音频当前架构

## 生产链路

用户态通过 Linux ALSA `/dev/snd/pcmC0D0p` 写入独立 playback 子流；`kernel::audio::engine`
把运行中的子流混成一个物理 period，经 `drivers::PcmOutput` 交给 VirtIO Sound device ID 25。
没有用户态混音服务、私有音频 IPC 或 Web media runtime，当前产品也没有媒体播放器。

## 状态与调度

- 卡提供 8 个软件 playback 子流；每次 open 占用首个空槽，OFD 独占状态、应用指针和全新 ring backing。
- PCM 固定 48 kHz、双声道、F32_LE，period 256 帧、buffer 1024 帧；支持 RW 与 mmap interleaved。
- 卡级 `Engine` 独占物理流、子流槽位、在途 period 的 contributor mask 和 Master；物理流在卡初始化时
  configure/prepare 一次，各子流的 HW_PARAMS/PREPARE 不重配设备。
- 应用提交或设备完成触发 tick，目标硬件队列深度为 2 个 period；完成时只推进实际贡献子流的 hw_ptr。
- `mixer::Accumulator` 逐样本相加，NaN/Inf 按静音处理，施加左右 Master 增益后硬削波到 [-1, 1]。
  音量 0..100 按平方曲线映射，默认 100 且开启；没有 lookahead limiter 或持久化音量。
- `/dev/snd/controlC0` 发布卡信息、PCM 枚举和 Master Playback Volume/Switch；当前没有 per-substream 音量元素。

## 失败与当前限制

OFD close 清除该子流在所有在途 period 的归属，再归还槽位，旧 mmap backing 不会与新 open 共享。
物理设备失败使整张卡断开，poll 返回错误/挂断；单子流欠载进入 XRUN。

ALSA control 事件队列和偏好子设备选择尚未实现：当前 SUBSCRIBE_EVENTS 写回未订阅状态，
PCM_PREFER_SUBDEVICE 不改变首空槽分配。它们不构成完整 ALSA control 语义，后续需修正后才能宣称兼容。
PCM 仍缺少 TTSTAMP、USER_PVERSION、REWIND、RESET 等 Wine consumer 所需操作。

AArch64/HVF 是声音 runtime 的目标；当前只有 host 状态/codec/数学测试及编译覆盖，没有多路真实输出或
amixer runtime 通过证据。RISC-V 复用源码，只有编译与静态覆盖。
