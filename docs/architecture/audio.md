# 音频当前架构

## 生产链路

用户态通过 Linux ALSA `/dev/snd/pcmC0D0p` 写入独立 playback 子流；`kernel::audio::engine`
把运行中的子流混成一个物理 period，经 `drivers::PcmOutput` 交给 VirtIO Sound device ID 25。
没有用户态混音服务、私有音频 IPC 或 Web media runtime，当前产品也没有媒体播放器。
桌面的系统提示音由 `user/libraries/sound` 直接写同一个 PCM：登录完成、注销、清空回收站各播一段，只有本地提取了原版素材时才安装，
设备不可用（例如非 root 账户没有 `/dev/snd` 权限）时静默跳过。

## 状态与调度

- 卡提供 8 个软件 playback 子流；默认 open 占用首个空槽，也可由同一 opener TID 的 control OFD
  指定子设备。OFD 独占状态、应用指针和全新 ring backing。
- PCM 固定 48 kHz、双声道、F32_LE，period 256 帧、buffer 1024 帧；支持 RW 与 mmap interleaved。
- 卡级 `Engine` 独占物理流、子流槽位、在途 period 的 contributor mask 和 Master；物理流在卡初始化时
  configure/prepare 一次，各子流的 HW_PARAMS/PREPARE 不重配设备。
- 应用提交或设备完成触发 tick，目标硬件队列深度为 2 个 period；完成时只推进实际贡献子流的 hw_ptr。
- `mixer::Accumulator` 对每路先施加左右 PCM 增益再逐样本相加，NaN/Inf 按静音处理，
  施加左右 Master 增益后硬削波到 [-1, 1]。静音分支直接输出零，即使有限样本求和已溢出，静音也不会产生 NaN。
  音量 0..100 按平方曲线映射，默认 100 且开启；没有 lookahead limiter 或持久化音量。
- `/dev/snd/controlC0` 发布卡信息、PCM 枚举、Master Playback Volume/Switch 和 8 个按 index 区分的
  PCM Playback Volume。PCM 音量属于卡上的固定元素，关闭、重新打开子流不会复位。
- control 订阅属于 OFD，最多排队 10 个元素，首次变化 FIFO，同一元素未读前的变化合并。
  read/poll 通过独立 readiness pipe 等待，PCM completion 不唤醒空闲 mixer。
- PCM 支持 USER_PVERSION、TTSTAMP，以及启用后的 STATUS/SYNC_PTR 参考时钟快照。
  realtime 使用 timer 的 RTC offset；MONOTONIC/RAW 共用当前未校频的 counter。SW_PARAMS 可在
  Prepared/Running 状态更新；状态时间戳不提供硬件级采样精度或 trigger timestamp。

## 失败与当前限制

OFD close 清除该子流在所有在途 period 的归属，再归还槽位，旧 mmap backing 不会与新 open 共享。
物理设备失败使整张卡断开，poll 返回错误/挂断；单子流欠载进入 XRUN。

指定槽位忙时返回 EBUSY，不回退到其他槽；control actual fput 立即撤销偏好与订阅，dup 保留同一 OFD。
PCM 的阻塞 open 等待、RW 自动启动、任意 frame 长度传输、REWIND、RESET、DRAIN、STATUS_EXT 等仍未提供；
这些范围不能由显式启动、整 period 写入的探针通过来替代，Wine consumer 尚待后续验证。

AArch64/HVF 是声音 runtime 的目标。`verify-runtime-audio` 使用固定上游 alsa-lib/amixer，
冷启动私有镜像，运行八路 consumer 并分析设备实际 WAV 的频率、左右增益、静音和硬削波。
其六秒 workload 要求无 XRUN；host mixer benchmark 仅作数学 CPU 成本诊断，不冒充 softfloat 设备延迟。
RISC-V 复用源码，只有编译与静态覆盖。
