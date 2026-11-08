# 音频当前架构

## 生产链路

用户态通过 Linux ALSA 访问声卡：支持 48 kHz 双声道的输出发布 `pcmCND0p` 桌面共享混音，
`pcmCND1p` 提供原生格式独占输出。共享应用固定写 F32，整数 DAC 的格式转换归 audio-core。
原生 `audio-core`（Fuchsia audio_core）经 `device-protocol` 独占每个 PCM 输出（驱动宿主内的 VirtIO Sound 与
xHCI/UAC2 adapter），拥有混音引擎、主音量与独占格式切换；linuxd 只翻译 ALSA：每个已配置的子流是 audio-core 的一个
渲染流（`audio-protocol`）。应用 mmap ring 由 linuxd 的 VMO 支撑并交给 audio-core 读取；应用指针经共享控制区发布，
已播放帧数、欠载与断开经同一控制区回到 ALSA 状态，热路径不走 RPC。混音后的固定 period 由 audio-core 经 PCM 请求
channel 交给设备；DMA 固定、硬件 queue 与 reset 由驱动宿主独占。
媒体播放机经 libmpv/FFmpeg 解码、alsa-lib 写入同一 PCM；混音由 audio-core 完成，应用只使用标准 ALSA UAPI。
桌面的系统提示音由 `user/libraries/sound` 直接写同一个 PCM：登录完成、注销、清空回收站各播一段，只有本地提取了原版素材时才安装，
设备不可用（例如非 root 账户没有 `/dev/snd` 权限）时静默跳过。

macOS 图形入口由 `scripts/utm_runtime.py` 通过 UTM 原生 `Sound` 配置统一装配 VirtIO Sound、
音频后端与客户端回放；附加参数只固定 `streams=1`，不重复添加声卡或后端。
UTM 只在 `Sound` 非空时启用客户端音频；空列表配合手工 QEMU 声卡会造成 guest 正常播放而宿主无声。

## 状态与调度

- 卡提供 8 个软件 playback 子流；默认 open 占用首个空槽，也可由同一 opener TID 的 control OFD
  指定子设备。OFD 独占状态、应用指针和全新 ring backing。
- 共享端点固定 48 kHz、双声道、F32_LE、period 256 帧；软件 buffer 经标准 HW_REFINE/HW_PARAMS
  协商为 4..=32 个 period（1024..=8192 帧），支持 RW 与 mmap interleaved。
  物理 DMA 队列仍为 4 个 period；软件 ring 不再受其 21ms 容量限制，普通播放器可申请约 100ms 缓冲。
- 每个 adapter 对应一张卡；共享端点与独占端点互斥。独占端点的 HW_REFINE/HW_PARAMS 与实际设备能力求交，
  按协商采样率、位深、声道与 period 配置物理流，提交原始 PCM bytes，不经过软件混音、增益或重采样。
  configure/release 等冷路径由 audio-core 输出的配置锁串行化，不持引擎短锁等待设备。
- （audio-core）应用提交或设备完成触发 tick，最多预送物理缓冲容量减一的 3 个 period（16 ms），保留一段应用数据跨越批量完成；
  实际混音/提交仅由每输出的 mixer 执行；控制 RPC 更新状态后投递已有端口，不能在无预算的 RPC 线程混音。
  mixer 按实际物理 period 申请 10% CBS/EDF 预算，配置变化先重新准入；失败明确停止输出并报告错误，不静默退回公平类。
  完成时只推进实际贡献子流的 hw_ptr。全部预送会在整批 TX 完成、写进程尚未补充时提前耗尽应用数据并报告 XRUN。
  批量完成后先补填队列，补填后仍为空才停止并重新 prepare；否则连续播放会随每批完成反复停启物理流。
  只预送 2 个 period 的约 10.7 ms 余量不足以覆盖 QEMU 默认 10 ms 音频消费与补充延迟，会使宿主回放断音。
  预送时若某一路暂缺下一段、但仍有在途数据，等待该流补充；仅在它的数据真正耗尽后报告 XRUN，再让其他流继续。
  WRITEI 与 mmap 应用指针允许非整周期增量；短批次在本流在途数据耗尽后提交，物理周期剩余部分静音，完成只计入实际帧数。
- `mixer::Accumulator` 对每路先施加左右 PCM 增益再逐样本相加，NaN/Inf 按静音处理，
  施加左右 Master 增益后硬削波到 [-1, 1]。静音分支直接输出零，即使有限样本求和已溢出，静音也不会产生 NaN。
  音量 0..100 按平方曲线映射，默认 100 且开启；没有 lookahead limiter 或持久化音量。
- `/dev/snd/controlC0` 发布卡信息、PCM 枚举、Master Playback Volume/Switch 和 8 个按 index 区分的
  PCM Playback Volume。PCM 音量属于卡上的固定元素，关闭、重新打开子流不会复位。
- control 订阅属于 OFD，最多排队 10 个元素，首次变化 FIFO，同一元素未读前的变化合并。
  read/poll 通过独立 readiness pipe 等待，PCM completion 不唤醒空闲 mixer。
- PCM 支持 USER_PVERSION、TTSTAMP，以及启用后的 STATUS/SYNC_PTR 参考时钟快照。
  STATUS_EXT 读取标准音频时间戳配置字段，返回同一参考时钟快照，不声明硬件音频时间戳或精度。
  realtime 使用 timer 的 RTC offset；MONOTONIC/RAW 共用当前未校频的 counter。SW_PARAMS 可在
  Prepared/Running 状态更新；状态时间戳不提供硬件级采样精度或 trigger timestamp。

## 失败与当前限制

原生端点只保证 guest 到 adapter 的 PCM 保真；UTM/SPICE 出口仍可能转换格式。USB DAC 路径直接发送原生 PCM；macOS 独占后端尚未实现，VirtIO/UTM 音频路径不能据此宣称端到端 bit-perfect。

OFD close 清除该子流在所有在途 period 的归属，再归还槽位，旧 mmap backing 不会与新 open 共享。
物理设备失败使整张卡断开，poll 返回错误/挂断；单子流欠载进入 XRUN，WRITEI 返回 EPIPE，客户端可 PREPARE 恢复。

指定槽位忙时返回 EBUSY，不回退到其他槽；control actual fput 立即撤销偏好与订阅，dup 保留同一 OFD。
PCM 的阻塞 open 等待、RW 自动启动、REWIND、RESET、DRAIN 等仍未提供；
这些范围不能由显式启动、整 period 写入的探针通过来替代。

AArch64/HVF 是声音 runtime 的目标。`verify-runtime-audio` 使用固定上游 alsa-lib/amixer，
冷启动私有镜像，运行八路 consumer 并分析设备实际 WAV 的频率、左右增益、静音和硬削波。
其六秒 workload 要求无 XRUN；host mixer benchmark 仅作数学 CPU 成本诊断，不冒充 softfloat 设备延迟。
RISC-V 复用源码，只有编译与静态覆盖。

## UTM USB DAC 使用

`make run-gui` 通过标准 usbredir 自动转发 USB Audio 2 输出设备，设备由 guest 独占；宿主授权和会话清理见[构建与运行](../development/build-and-verify.md)。USB2 high-speed DAC 仍通过 xHCI 的 USB2 root port 工作。
在「控制面板 → 声音和多媒体 → 音频 → 声音播放」选择首选设备，点「应用」或「确定」后保存为标准
ALSA 默认播放和 control 配置。普通播放器、系统提示音和音量控制在新打开流或控件时读取它；
已打开的流继续使用原设备。选定端点不可用时，在同一界面重新选择可用设备。
媒体播放机还可以明确选择「原生独占」USB DAC，对应 `/dev/snd/pcmCND1p`。D&A Alpha 的 24/32 位 PCM 与采样率按描述符及 clock range 发布，不按型号硬编码。

Alpha 的 48 kHz 播放已在相同输入的原版/修补 QEMU 原生 libusb 路径对照验收；usbredir 产品链路、切换采样率、长时间无丢样及拔插仍需实机验收。
