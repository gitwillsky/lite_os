# LiteOS 文档索引

本文件是仓库文档的唯一总索引。每个事实只由下列一个文档 owner 维护；其他文档只链接，不复制。

## 架构

- [当前架构总则](architecture.md)
- [启动与平台](architecture/boot-platform.md)
- [执行、CPU、trap、timer 与同步](architecture/execution.md)
- [内存](architecture/memory.md)
- [进程与调度](architecture/process-scheduling.md)
- [文件系统与存储](architecture/filesystem-storage.md)
- [IPC 与网络](architecture/ipc-network.md)
- [设备与终端](architecture/devices-terminal.md)
- [音频](architecture/audio.md)
- [图形会话与 LiteUI](architecture/lite-ui.md)
- [用户态与 ABI](architecture/userspace-abi.md)

## 架构契约

- [全局 module、依赖与接口契约](architecture-contract.md)
- [启动与平台契约](architecture-contract/boot-platform.md)
- [执行域契约](architecture-contract/execution.md)
- [内存契约](architecture-contract/memory.md)
- [进程与调度契约](architecture-contract/process-scheduling.md)
- [文件系统与存储契约](architecture-contract/filesystem-storage.md)
- [IPC 与网络契约](architecture-contract/ipc-network.md)
- [设备与终端契约](architecture-contract/devices-terminal.md)
- [音频契约](architecture-contract/audio.md)
- [图形会话与 LiteUI 契约](architecture-contract/lite-ui.md)
- [用户态与 ABI 契约](architecture-contract/userspace-abi.md)

## Linux 64-bit 用户态 ABI

- [syscall 支持总则](syscall-support.md)
- [进程与身份](syscall-support/process-identity.md)
- [内存](syscall-support/memory.md)
- [文件系统与 I/O](syscall-support/filesystem-io.md)
- [同步与调度](syscall-support/synchronization-scheduling.md)
- [信号与时间](syscall-support/signal-time.md)
- [IPC](syscall-support/ipc.md)
- [Socket](syscall-support/socket.md)
- [系统](syscall-support/system.md)

## 工程基线

- [固定规范与上游来源](standards-baseline.md)
- [构建、测试与验证](development/build-and-verify.md)
- [生成的 scoped interface baseline](generated/architecture-interface.txt)

## 设计决策与术语

- [首个声音里程碑只支持播放](decisions/audio-output-scope.md)
- [首个声音 Web API 使用 HTMLMediaElement](decisions/audio-web-api-surface.md)
- [Web 媒体播放组合 Native 文件能力](decisions/audio-native-file-capability.md)
- [系统音频服务独占输出并混合逻辑 stream](decisions/system-audio-service.md)
- [Native 音频边界使用 Linux ALSA PCM UAPI](decisions/alsa-pcm-native-abi.md)
- [首期音频硬件只使用 VirtIO Sound](decisions/virtio-sound-backend.md)
- [LiteUI audio worker 独占媒体解码](decisions/media-decoder-owner.md)
- [在共享 mixer 下追求透明且 CPU 有界的播放](decisions/audio-quality-policy.md)
- [音频链路固定 48 kHz stereo float PCM](decisions/audio-pcm-normal-form.md)
- [首期启用 Symphonia 0.6.0 全部稳定音频能力](decisions/audio-codec-baseline.md)
- [首次有声播放需要应用内用户激活](decisions/audio-autoplay-policy.md)
- [AF_UNIX 控制面与 memfd SPSC PCM ring 分离](decisions/audio-stream-transport.md)
- [音频 render quantum、buffer 与延迟预算](decisions/audio-latency-budget.md)
- [系统音量由 mixer 独占并只向 desktop 开放控制](decisions/system-audio-controls.md)
- [playback stream 使用 per-app 与 session 固定配额](decisions/audio-resource-limits.md)
- [声音能力以可分析输出和真实全链路 gate 完成](decisions/audio-verification-gates.md)
- [音频使用独立 kernel 与 userspace owner 链](decisions/audio-domain-owners.md)
- [每个 LiteUI 进程复用一条 audio control connection](decisions/audio-client-protocol.md)
- [首期交付只使用公开能力的 Music Player](decisions/music-player-acceptance-app.md)
- [术语表](glossary.md)

完成的计划、阶段快照和已被当前文档吸收的设计草案不保留在工作树中；需要追溯时使用 Git 历史。
