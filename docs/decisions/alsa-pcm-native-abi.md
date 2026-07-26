# ADR: Native 音频边界使用 Linux ALSA PCM UAPI

- 状态：已接受
- 日期：2026-07-26

## 背景

系统音频服务需要稳定的 kernel/user playback 边界，Native 程序也需要标准音频能力。自定义
`/dev/audio`、私有 ioctl 或直接暴露 VirtIO descriptor 会建立 LiteOS 私有 ABI，并把具体 adapter
状态泄漏到用户态。

## 决策

kernel 发布 Linux ALSA PCM playback UAPI，设备身份为 `/dev/snd/pcmC0D0p`。首个里程碑只实现
系统音频服务实际消费的 playback 子集；支持的 ioctl、layout、format、poll/mmap/read-write 行为和
errno 必须逐项登记在 syscall/ABI 矩阵，未实现操作显式失败。

UAPI layout 与编号固定到 [Linux ABI 基线](../standards-baseline.md#linux-arm64--riscv64-abi)；
VirtIO Sound 只属于 kernel adapter，不进入 ALSA 用户态契约。

## 结果

- 系统音频服务与 Native consumer 使用标准 Linux ABI，不依赖 project-private kernel protocol。
- kernel ALSA PCM owner 负责 open、参数、prepare/start/drop、ring position、poll readiness、xrun 和 close。
- 不能只让 ioctl 返回成功而缺少对应状态转换；ABI 子集必须有 codec、状态模型和 target runtime consumer。
- capture/control/MIDI/sequencer 不因 ALSA 名称自动进入首个里程碑。
