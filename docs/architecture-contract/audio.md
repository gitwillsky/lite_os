# 音频契约

本文 `audio`、Card、PCM/control 文件均指 `linuxd::devices::audio`。硬件 adapter 位于 driver-host，
`PcmOutput` 的 client/server 由 device-protocol 的 PCM channel 连接；应用不调用系统服务协议。

## Owner 与依赖

| Owner | 唯一职责与调用者 |
|---|---|
| `drivers::usb` | UAC2 clock/alternate、反馈与等时队列；实现同一 PCM seam |
| `drivers::virtio::sound` | 物理流 command、DMA、queue、completion 与 reset；实现 `devices::model::PcmOutput` |
| `audio::Card` | 每个实际 adapter 的卡、observer、completion counter、Engine mutex、control 弱引用 registry 与两条 readiness pipe |
| `audio::engine` | 纯状态机；8 个子流、物理队列、Master/PCM 音量、各流贡献帧数，由 Card 串行调用 |
| `audio::mixer` | period float accumulation、声道增益与硬削波，由 Engine 调用 |
| `audio::state` | 单个子流的 ALSA state、应用/hardware 指针、软件参数与 xrun |
| `audio::codec` / `ioctl` | PCM Linux layout、INFO/HW_REFINE/参数/status/pointer codec 与 user-copy |
| `audio::control` / `control_file` | control layout、10 个固定元素、每 OFD 的订阅/FIFO/偏好、卡信息和 PCM 枚举 |
| `audio::pcm_file` | PCM cdev open 与 OFD 的设备操作投影 |
| `platform` / `fs` | machine facts / 按已注册设备号发布 PCM 与 control 节点 |

PCM/control ioctl 的唯一 caller 是已注册 DeviceFile dispatch，不经过 syscall 中的设备专用分支。
`PcmFile::write_frames` 只由 PCM ioctl 调用，返回实际写入帧数或原生 errno；XRUN 返回 EPIPE，
不能进入仅处理 EAGAIN 的等待重试。`Engine::application_frames` 只由 PcmFile 的 WRITEI/mmap 提交调用；
`PcmStateOwner::submit_frames` 与 `complete_period(frames)` 只由 Engine 推进真实帧数。
`codec::SNDRV_PCM_IOCTL_STATUS_EXT` 只由 ioctl 分派消费，与 STATUS 共用参考时钟快照。
`PcmStateOwner::buffer_frames` 独占已协商的软件容量；HW_REFINE 同时约束 period 数、buffer size/bytes/time，
HW_PARAMS 按 Linux 顺序选择首个 access/format/subformat 和最大可行 buffer，再投影关联参数。
Engine 混音、WRITEI、mmap 与 status 只消费同一容量。软件容量由 state 按当前配置限定；共享端点上限 64 KiB，原生端点按最大 PCM period 预留有界 backing，
open 预留 backing 后不重分配，协商不改变物理 DMA 队列；旧 mmap 保活原 OFD 的 backing，不与新 OFD 共享。
`PcmInfo`/`CardText` 只向 codec 提供卡与子流快照；`PcmFile::subdevices` 向 INFO 提供槽位、总数和空闲数。
通用内核不包含 ALSA、mixer 或媒体协议。

## 接口与所有权证明

1. `Card.engine` 是物理队列、子流状态与 Master/PCM 音量的唯一锁；OFD operation mutex 串行化同一 OFD 的控制与提交，
   同时拥有该 OFD 的 timestamp clock 选择与启用状态。锁序为 operation → Engine，状态快照也遵守此顺序。
   持 Engine 锁可能调用 adapter reclaim，因此 observer 只能累加 Atomic 完成计数和断开标志，不能重入 Engine。
   Card.process 在锁内消费事件并 tick，锁外发 readiness；缺少该顺序会形成回调持锁自等。
   Engine 处理完成事件后必须先补填，再判断物理流是否空闲；TX 批量归还不等于应用欠载，提前 STOP 会打断连续 PCM。
   预送深度为 `PCM_PERIODS - 1`，保留一段应用数据跨越整批 TX 完成；额外缩短深度会使宿主断粮，全部预送会提前耗尽应用数据。
   缺少未来 period 的运行中子流若仍有在途贡献，不得提前混入缺口或置 Xrun；等待补充或在途耗尽后再裁决。
2. 全局 `DISCOVERY` 只发布一次，按设备注册表的只追加顺序附加实际 PCM outputs，每张卡独占 observer 与 Engine。Discovery 聚合远端卡级 completion，由 linuxd 的 deferred 服务线程分派；重复 USB 拔插不会耗尽硬件源表。
   共享与原生端点互斥；原生 HW_PARAMS/HW_FREE/CLOSE 经 `Card.configuration` 的 TaskMutex 串行化，
   Engine 短锁 只保护状态，adapter configure/release 等等待发生在其外。原生提交不经过 Accumulator。
   缺少卡级配置事务会让共享恢复和独占重配交错，或让 completion 等待自身持有的 Engine 锁。
3. 每次 open 分配新 DeviceBacking 和 mmap identity，再占槽、发布 OFD；OFD 分配失败回滚槽位。
   每个在途物理 period 保存各子流的有效帧数；短批次补零不增加应用/hardware 指针。
   close/prepare/drop 清除该流的在途帧数，旧 completion 不得推进复用槽位的 hw_ptr。
   backing 在 Engine 锁外销毁，旧映射可继续持有原 backing，但不能访问新 open 的音频。
4. `linuxd::main` 装配实际调用 TID observer，audio 不依赖 task；control open 保存 `Arc<PidIdentity>`，
   偏好只匹配原 opener 的 identity，数字 TID 复用、fd 传递或其他线程不能继承它。
   `Card.controls` 只存 Weak，锁序 registry → OFD state；不得跨它们持 Engine 或调用 readiness。
   actual fput 的 closed 状态立即撤销观察能力，backend Arc 或 wait observer 不得延长偏好生命期。
   open 时清理失效条目，registry 分配失败返回 ENOMEM、不发布 OFD；没有 per-event 堆分配。
5. 每个 control OFD 的有界 FIFO 是订阅状态的唯一 owner；取消订阅清空，重复订阅保留事件。
   ELEM_WRITE 先完整校验两声道再提交，实际变更才通知；提交后的 copyout 失败保留变更及事件。
   先释放 registry/state/Engine 再通过 control 专用 pipe 唤醒；否则 poll 回调可重入死锁。
   事件出队前预检用户输出，实际 user-copy 不持音频锁；空队列的 EAGAIN 优先于无效目标地址。
   control/PCM prepare_wait 都必须先查 level、排空旧 token、再查 level；否则阻塞 read/WRITEI
   可在服务线程中因 stale token 自旋，阻止同 CPU writer 或 completion 所需的调度。

`Engine<Ring>` 的窄接口由 Card/PcmFile 使用，host fixture 注入假 ring/output 验证相同状态机；
`Accumulator` 只由 Engine 消费。steady tick 使用固定栈数组与预分配 ring，不引入 period 级堆分配。
短帧记账使用固定大小的每流帧数数组，跨 ring 边界至多两次复制；短尾补零不增加锁或分配。
共享端点的 ring 每 OFD 最多 16 页（64 KiB），八个在用子流共 512 KiB；HW_REFINE 在冷路径最多枚举 29 个配置，
不增加混音/提交路径的锁、分配或扫描。沿用八路 WAV 门禁，并用真实 libmpv 的协商与欠载行为验证供数余量。
成本验证沿用八路零 XRUN 与实际 WAV 门禁，不为该分支另设时间阈值；mixer 算法与其 CPU 基准不变。
逐流增益的新增成本由生产 Accumulator 的 host diagnostic benchmark 与 AArch64/HVF 八路无 XRUN
workload 分别观察；不以 host 硬件浮点计时宣称 linuxd softfloat 延迟达标。门禁和证据范围见构建验证文档。

完成计数的消费与 Engine 状态推进属于同一引擎锁事务；每次只消费一个快照，锁外立即通知写者。
新的设备完成由回调发布的 AUDIO 工作位保活，不能恢复同步追赶循环，延迟应用补数直至 underrun。
八路 ALSA 实际播放与 WAV 门禁验证供数和混音，宿主数学 benchmark 不替代该验证。

`PcmOutput::submit_period` 原子准入 adapter 的有界队列：无额度返回 WouldBlock，Engine 不推进 mixed/queue。
每个物理 adapter 至少有四个 DMA period slots。RemotePcm 独占四个传输额度，涵盖请求 channel 中及设备中的全部
已接受 period；成功入队后只有对应代际的硬件完成归还额度。它不是硬件 slot 查询或镜像，不保留 writable 预查 RPC。
数据消息的事务号为零，设备线程按请求 channel 顺序消费；传输接管 bytes 后调用者可立即复用原缓冲。
设备拒绝已准入的数据必须发出断开事件，不能丢数据后继续报告成功。断开、重复完成或错误帧数永久关闭准入。

冷控制由 RemotePcm.control 串行化，先在原 observer mutex 扩展的 ClientState 事务中关闭准入，再发送同步请求。
提交的预留与非阻塞 channel write 在同一 ClientState 事务内，控制不能越过旧提交；写失败唯一归还该预留。
事件线程不取得 control 锁，先归还额度并克隆 observer，释放 ClientState 后才回调，不能被慢控制阻塞。
RELEASE 保持设备原有 DMA/回调屏障；成功后撤销客户端配置，迟到事件不能再推进引擎。下一次 CONFIGURE 使用
不回绕的代际；EventWriter 的 Mutex 独占事件端点与 generation，在旧 release 屏障后换代。
两条 channel 接收顺序独立，故旧代际完成必须忽略；物理断开则与代际无关，未配置时同样永久撤销连接。
EventHub 在事件 peer 关闭或重登记失败时交付一次终止错误并撤销 source，PCM 将它与错误格式消息统一视为断开；
否则四个额度耗尽后会永久等待一个已不存在的 producer，而非向应用交付设备失败。
EventWriter 发送失败时在其状态事务内撤销端点、锁外关闭；已排队消息消费完后交付终止，不能忽略失败而丢失额度。

Benchmark 决策：数据提交取消混音锁内的同步回复等待，使用既有请求 channel、事件线程与固定四个额度，不新增
工作线程或独立数据队列。新增的 control mutex 只用于冷操作，事件端点 mutex 串行发送/终止与换代；
代际与准入标志分别防止迟到完成和控制越过数据。
四个额度在途、迟到/重复完成与断开由实际客户端状态夹具保护；完整音频/WAV 门禁仍使用原八路、1024-frame 软件缓冲与零 XRUN 要求。

## 失败、范围与验证

- 无卡或 backing 分配失败不得发布 OFD；槽位耗尽返回 EBUSY。断开卡拒绝新的 PCM/control open，已有 control ioctl 返回 ENODEV；旧卡号和 OFD 不绑定到新连接。
- 子流 xrun、drop、close 与设备断开必须分别保留软件状态和在途完成的正确归属；断开不重连。
- 未登记 ioctl 返回 ENOTTY；control ID 不存在返回 ENOENT、值非法返回 EINVAL、user-copy 失败返回 EFAULT。
- 未订阅 read 返回 EBADFD、长度不足一个 72-byte 事件返回 EINVAL、空队列非阻塞 read 返回 EAGAIN。
  control write 不开放。偏好为 -1 自动选择，其余值精确匹配；无槽位返回 EBUSY。
- PCM 部分语义、capture、MIDI 和 sequencer 的范围见[当前架构](../architecture/audio.md)与 ABI 矩阵。
- host 测试覆盖混音数学、Engine contributor/close/underrun、PCM 与 control codec。
  runtime fixture 使用真实 Linux UAPI、上游 amixer 与 alsa-lib 八路播放。
