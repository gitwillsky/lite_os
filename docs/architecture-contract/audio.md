# 音频契约

## Owner 与依赖

| Owner | 唯一职责与调用者 |
|---|---|
| `virtio::sound` | 物理流 command、DMA、queue、completion 与 reset；实现 `drivers::PcmOutput` |
| `audio::Card` | 唯一卡、adapter observer、completion counter、Engine mutex、control 弱引用 registry 与两条 readiness pipe |
| `audio::engine` | 纯状态机；8 个子流、物理队列、Master/PCM 音量、贡献掩码，由 Card 串行调用 |
| `audio::mixer` | period float accumulation、声道增益与硬削波，由 Engine 调用 |
| `audio::state` | 单个子流的 ALSA state、应用/hardware 指针、软件参数与 xrun |
| `audio::codec` / `ioctl` | PCM Linux layout、INFO/HW_REFINE/参数/status/pointer codec 与 user-copy |
| `audio::control` / `control_file` | control layout、10 个固定元素、每 OFD 的订阅/FIFO/偏好、卡信息和 PCM 枚举 |
| `audio::pcm_file` | PCM cdev open 与 OFD 的设备操作投影 |
| `platform` / `fs` | adapter 装配 / 按已注册设备号发布 PCM 与 control 节点 |

PCM/control ioctl 的唯一 caller 是已注册 DeviceFile dispatch，不经过 syscall 中的设备专用分支。
`PcmInfo`/`CardText` 只向 codec 提供卡与子流快照；`PcmFile::subdevices` 向 INFO 提供槽位、总数和空闲数。
内核不依赖用户态 mixer 或媒体协议。

## 接口与所有权证明

1. `Card.engine` 是物理队列、子流状态与 Master/PCM 音量的唯一锁；OFD operation mutex 串行化同一 OFD 的控制与提交，
   同时拥有该 OFD 的 timestamp clock 选择与启用状态。锁序为 operation → Engine，状态快照也遵守此顺序。
   持 Engine 锁可能调用 adapter reclaim，因此 observer 只能累加 Atomic 完成计数和断开标志，不能重入 Engine。
   Card.process 在锁内消费事件并 tick，锁外发 readiness；缺少该顺序会形成回调持锁自等。
2. 全局 CARD Once 只在 init 发布一次；初始化 configure/prepare 物理流后安装 observer/completion source。
   子流不调用物理 release；`PcmOutput` 只保留 configure/prepare/start/stop/submit/writable。
   init 后物理流持续属于卡，子流 HW_FREE/CLOSE 只回收软件状态。
3. 每次 open 分配新 DeviceBacking 和 mmap identity，再占槽、发布 OFD；OFD 分配失败回滚槽位。
   close/prepare/drop 清除 contributor mask，旧 completion 不得推进复用槽位的 hw_ptr。
   backing 在 Engine 锁外销毁，旧映射可继续持有原 backing，但不能访问新 open 的音频。
4. `main` 装配实际调用 TID observer，audio 不依赖 task；control open 保存 `Arc<PidIdentity>`，
   偏好只匹配原 opener 的 identity，数字 TID 复用、fd 传递或其他线程不能继承它。
   `Card.controls` 只存 Weak，锁序 registry → OFD state；不得跨它们持 Engine 或调用 readiness。
   actual fput 的 closed 状态立即撤销观察能力，backend Arc 或 wait observer 不得延长偏好生命期。
   open 时清理失效条目，registry 分配失败返回 ENOMEM、不发布 OFD；没有 per-event 堆分配。
5. 每个 control OFD 的有界 FIFO 是订阅状态的唯一 owner；取消订阅清空，重复订阅保留事件。
   ELEM_WRITE 先完整校验两声道再提交，实际变更才通知；提交后的 copyout 失败保留变更及事件。
   先释放 registry/state/Engine 再通过 control 专用 pipe 唤醒；否则 poll 回调可重入死锁。
   事件出队前预检用户输出，实际 user-copy 不持音频锁；空队列的 EAGAIN 优先于无效目标地址。
   control/PCM prepare_wait 都必须先查 level、排空旧 token、再查 level；否则阻塞 read/WRITEI
   可在 kernel 中因 stale token 自旋，阻止同 CPU writer 或 completion 所需的调度。

`Engine<Ring>` 的窄接口由 Card/PcmFile 使用，host fixture 注入假 ring/output 验证相同状态机；
`Accumulator` 只由 Engine 消费。steady tick 使用固定栈数组与预分配 ring，不引入 period 级堆分配。
逐流增益的新增成本由生产 Accumulator 的 host diagnostic benchmark 与 AArch64/HVF 八路无 XRUN
workload 分别观察；不以 host 硬件浮点计时宣称 kernel softfloat 延迟达标。门禁和证据范围见构建验证文档。

## 失败、范围与验证

- 无卡或 backing 分配失败不得发布 OFD；槽位耗尽返回 EBUSY。
- 子流 xrun、drop、close 与设备断开必须分别保留软件状态和在途完成的正确归属；断开不重连。
- 未登记 ioctl 返回 ENOTTY；control ID 不存在返回 ENOENT、值非法返回 EINVAL、user-copy 失败返回 EFAULT。
- 未订阅 read 返回 EBADFD、长度不足一个 72-byte 事件返回 EINVAL、空队列非阻塞 read 返回 EAGAIN。
  control write 不开放。偏好为 -1 自动选择，其余值精确匹配；无槽位返回 EBUSY。
- PCM 部分语义、capture、MIDI 和 sequencer 的范围见[当前架构](../architecture/audio.md)与 ABI 矩阵。
- host 测试覆盖混音数学、Engine contributor/close/underrun、PCM 与 control codec。
  runtime fixture 使用真实 Linux UAPI、上游 amixer 与 alsa-lib 八路播放；Wine ALSA consumer 仍待验证。
