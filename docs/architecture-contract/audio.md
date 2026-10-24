# 音频契约

## Owner 与依赖

| Owner | 唯一职责与调用者 |
|---|---|
| `virtio::sound` | 物理流 command、DMA、queue、completion 与 reset；实现 `drivers::PcmOutput` |
| `audio::Card` | 唯一卡、adapter observer、completion counter、Engine mutex 与 readiness pipe |
| `audio::engine` | 纯状态机；8 个子流、物理队列、Master、贡献掩码，由 Card 串行调用 |
| `audio::mixer` | period float accumulation、声道增益与硬削波，由 Engine 调用 |
| `audio::state` | 单个子流的 ALSA state、应用/hardware 指针、软件参数与 xrun |
| `audio::codec` / `ioctl` | PCM Linux layout、INFO/HW_REFINE/参数/status/pointer codec 与 user-copy |
| `audio::control` / `control_file` | control layout、Master 元素、卡信息和 PCM 枚举的 DeviceFile 操作 |
| `audio::pcm_file` | PCM cdev open 与 OFD 的设备操作投影 |
| `platform` / `fs` | adapter 装配 / 按已注册设备号发布 PCM 与 control 节点 |

PCM/control ioctl 的唯一 caller 是已注册 DeviceFile dispatch，不经过 syscall 中的设备专用分支。
`PcmInfo`/`CardText` 只向 codec 提供卡与子流快照；`PcmFile::subdevices` 向 INFO 提供槽位、总数和空闲数。
内核不依赖用户态 mixer 或媒体协议。

## 接口与所有权证明

1. `Card.engine` 是物理队列、子流状态与 Master 的唯一锁；OFD operation mutex 串行化同一 OFD 的控制与提交。
   持 Engine 锁可能调用 adapter reclaim，因此 observer 只能累加 Atomic 完成计数和断开标志，不能重入 Engine。
   Card.process 在锁内消费事件并 tick，锁外发 readiness；缺少该顺序会形成回调持锁自等。
2. 全局 CARD Once 只在 init 发布一次；初始化 configure/prepare 物理流后安装 observer/completion source。
   子流不调用物理 release；`PcmOutput` 只保留 configure/prepare/start/stop/submit/writable。
   init 后物理流持续属于卡，子流 HW_FREE/CLOSE 只回收软件状态。
3. 每次 open 分配新 DeviceBacking 和 mmap identity，再占槽、发布 OFD；OFD 分配失败回滚槽位。
   close/prepare/drop 清除 contributor mask，旧 completion 不得推进复用槽位的 hw_ptr。
   backing 在 Engine 锁外销毁，旧映射可继续持有原 backing，但不能访问新 open 的音频。

`Engine<Ring>` 的窄接口由 Card/PcmFile 使用，host fixture 注入假 ring/output 验证相同状态机；
`Accumulator` 只由 Engine 消费。steady tick 使用固定栈数组与预分配 ring，不引入 period 级堆分配。
新内核混音路径需要目标架构 benchmark 决策与多路音频分析 gate；目前尚缺这些证据，不能据 host 数学测试宣称延迟达标。

## 失败、范围与验证

- 无卡或 backing 分配失败不得发布 OFD；槽位耗尽返回 EBUSY。
- 子流 xrun、drop、close 与设备断开必须分别保留软件状态和在途完成的正确归属；断开不重连。
- 未登记 ioctl 返回 ENOTTY；control ID 不存在返回 ENOENT、值非法返回 EINVAL、user-copy 失败返回 EFAULT。
- 当前 control 事件订阅与偏好子设备语义不完整，范围见[当前架构](../architecture/audio.md)。
  不得把写回零或忽略 hint 的行为算作完整实现；per-substream gain、capture、MIDI 和 sequencer 未开放。
- host 测试覆盖混音数学、Engine contributor/close/underrun、PCM 与 control codec。
  真实多路设备输出、amixer 和 Wine ALSA consumer 仍是待办；ABI 范围以矩阵为准。
