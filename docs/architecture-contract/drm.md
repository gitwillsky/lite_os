# DRM/KMS 与图形资源契约

`linuxd::devices::drm` 拥有 Linux KMS 对象、OFD、事件及资源使用期；driver-host 拥有实际队列、硬件命令与完成。
通用设备接口及 DMA/中断约束见[设备契约](devices-terminal.md)，Wayland 内容与呈现见 [UI 契约](ui-framework.md)。

## KMS 对象与查询

- `drm::kms` 定义单 CRTC、encoder、connector 与 primary plane 的静态 mode object/schema；动态 framebuffer/blob ID 从静态范围之后分配。
  编号只属于该 device 生命周期，使用者必须通过标准查询取得；不能依赖固定 framebuffer/属性 ID 或与 GEM/host resource 混用。
- `DrmFile.universal_planes` 是标准 per-OFD client capability，仅影响 GETPLANERESOURCES：默认只显示 overlay（当前没有），启用后显示 primary。
  dup/fork 共享同一 OFD，独立 open 不继承；不改变 master/hardware。当前 cursor 仍经 CURSOR2，不公布未接通的 atomic cursor plane。
- GETPLANE 从唯一已接受逻辑配置读取 framebuffer/CRTC，possible_crtcs 是 CRTC 索引位图；格式只公布现有 ADDFB2 接受的 XR24。
  OBJ_GETPROPERTIES 校验 object ID/type：未知/类型不符 ENOENT，已存在但无属性集合 EINVAL；普通客户端只看到非 atomic 属性。
  GETPROPERTY 独立返回范围/类型/标准枚举元数据，type 是 IMMUTABLE ENUM，值为 Primary，枚举包含 Overlay/Primary/Cursor。
- SET_CLIENT_CAP(ATOMIC) 按 OFD 协商并开启 universal planes；未协商时隐藏 atomic 属性且拒绝 MODE_ATOMIC。
  LP64 ioctl/值/枚举结构由 user/linux-uapi 独占，typed wrapper 按实际 count 两次查询，列表增长重试；不暴露私有应用 ABI。
  语义对照固定 Linux 的 [drm_plane.c](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/drivers/gpu/drm/drm_plane.c) 与
  [drm_mode_object.c](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/drivers/gpu/drm/drm_mode_object.c)。

## KMS property blob

- `drm::property::Blob` 独占不可变载荷，DrmFile.blobs 持创建者强引用，DrmDevice.blobs 仅按 mode object ID 弱索引。
  framebuffer 与 blob 共用 ObjectIdReservation；未发布 ID 回滚，已发布 ID 不复用。没有独立的重号命名空间。
- CREATEPROPBLOB 先复制载荷、准备 Arc/节点/ID，再写回用户 ID；成功后无分配发布，失败先析构 blob 再回收 ID。
  PreparedUserBlob 的临时引用跨过 OFD/global 两步发布，防止并发 DESTROY 在弱索引发表之前退休最后引用。
  所有最后强引用在 OFD/device 锁外释放；Blob.Drop 摘除全局弱节点，不保活 device、不遗留历史索引。
- GETPROPBLOB 可按全局 ID 查询，仅在给定长度精确匹配时复制；探测返回真实长度，不按容量截断。
  DESTROY 只撤销本 OFD 创建者引用：缺失 ENOENT、非创建者或已释放 EPERM；close 锁外移交全部创建者引用。
  pending/active 配置持 Arc 跨过用户 DESTROY，不靠 blob ID 再查找；legacy SETCRTC 的模式也使用全局可查的 state blob，不建立伪用户创建者引用。
- 长度受有符号分配大小减对象开销约束；空载荷 EINVAL，分配失败 ENOMEM。仅 primary node 支持，不向 render node 开放 KMS。
  blob CRUD 不要求 master；DIRTYFB 与 page flip/modeset 一样必须由当前 master 提交，不能仅凭自己拥有 framebuffer 修改输出。
  语义对照固定 Linux 的 [drm_property.c](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/drivers/gpu/drm/drm_property.c)。
  KMS property blob 与尚未支持的 VirtIO-GPU resource blob 是不同对象。

## 原子配置与输出损伤

- `kms::Configuration` 是 mode、ACTIVE、connector 路由与 primary FB/CRTC/几何/damage 的唯一不可变组合；没有 framebuffer 不等于没有配置。
  completion.pending 的 Commit 与已确认 active 之间移交同一配置；查询/TEST_ONLY 读取已接受逻辑状态，实际像素/光标使用已确认扫描状态。
  legacy SETCRTC/PAGE_FLIP/disable 与 MODE_ATOMIC 共用这条提交和完成链，没有第二套 KMS 状态。
- `ioctl::atomic` 在锁外逐项读取标准 56-byte 请求及数组；对象/附着属性错误为 ENOENT，immutable/范围/组合错误为 EINVAL。
  重复赋值逐次验证、最后值生效；Changes 按真实硬件 schema 固定存储，不对用户对象/属性数添加私有上限。
  framebuffer/mode/damage 在准备期固定真实引用，失败与被覆盖载荷在 completion 锁外退休。master 变更与提交复核共用 completion 锁序。
- 支持 TEST_ONLY、NONBLOCK、ALLOW_MODESET 与 PAGE_FLIP_EVENT；TEST_ONLY 不发表/不触碰硬件，不能与 EVENT 合用，可以与 NONBLOCK 合用。
  活动、路由或 timing 改变需要 ALLOW_MODESET；NONBLOCK busy 返回 EBUSY，阻塞提交按原 readiness 等待并重新应用变更，不能覆盖后来接受的配置。
  空更新不产生事件；请求 EVENT 时必须包含有效状态更新。event 队列额度耗尽返回 ENOMEM，不能当作硬件 busy 永久等 IRQ。
- 当前仅一个 XR24 primary，原生 adapter 只支持全屏、1:1、无裁剪/缩放；不支持的几何、ASYNC 和未知 flags 拒绝。
  mode 仅接受产品范围的 canonical CVT timing；type/name/vrefresh 按 Linux 提示语义处理，GETCRTC 归一化而 GETPROPBLOB 保留原载荷。
  inactive mode 与 active blank 分别可表达；cursor 仍由 CURSOR2 提供，不公布 atomic cursor/显式 fence/其他 plane 能力。
- FB_DAMAGE_CLIPS 是标准四个 s32 的 drm_mode_rect 数组。每次提交重新建立 damage；未指定/零 ID 表示全幅，不能复用上一帧提示。
  新源/模式或首次输出强制全幅；否则裁剪并合并输出损伤，空并集只绑定目标、不向主机发刷新。
  `GraphicsDevice::submit_virgl_scanout` 同步传递可选矩形；driver-host 在同一批中发布 SET_SCANOUT 与非空损伤 RESOURCE_FLUSH，最终阶段统一报告 exact completion。
  参考固定 Linux 的 [atomic UAPI](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/drivers/gpu/drm/drm_atomic_uapi.c)、
  [damage helper](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/drivers/gpu/drm/drm_damage_helper.c) 与同树 drm_atomic_state_helper.c、drm_modes.c。

## Framebuffer 发布与提交寿命

- DRM CREATE_DUMB/ADDFB 必须先预留 backing、object node 与 identity；完整 ioctl copyout 成功后才
  无分配发布 handle/ID；identity reserve 同时预留 rollback node，copyout failure 必须按任意并发
  顺序无分配回收全部未发布 identity，已经 publication 的 identity 永不复用。
- DRM connector preferred mode 与 completion-confirmed active CRTC mode 是两个 generation；
  VirtIO-GPU display-info 更新不得使旧 active mode 的合法 PAGE_FLIP/DIRTYFB 失效。同步
  SETCRTC/DIRTYFB/RMFB 遇到内部 display-info 或另一 display operation 时必须按 device readiness
  generation 睡眠并重提；无 mode 变化的 display-info completion 也必须发布 readiness，禁止向
  userspace 泄漏 adapter `WouldBlock` 或发生 lost wake。
- `DRM_IOCTL_MODE_CURSOR2` 是硬件光标的唯一 UAPI：shape update 必须先完成标准
  `RESOURCE_CREATE_2D`/`RESOURCE_ATTACH_BACKING`/`TRANSFER_TO_HOST_2D` transaction，再提交
  cursorq 并等待 exact completion；不得把 VirGL 3D texture 直接交给 cursorq。纯 move 只向 queue 1 发布或覆盖 latest
  position，立即返回。两者都不得修改 active framebuffer、生成 page-flip event 或排到 VirGL controlq
  后面。
- Framebuffer 本身是不可变的引用计数 mode object，包含 identity、创建者、布局与 backing；namespace、pending transaction、active scanout 持有同一对象。
  创建时一次 fallible Arc 分配，copyout 后发布；普通翻页只转移/保留引用，不再凭裸 ID 推断资源存活。
  GETRESOURCES 仍只枚举 namespace；关闭可先撤销名字，已发表的硬件事务由实际引用保活到 exact completion/reset。
- completion 先把 pending 对象移交 active 或退休，把被覆盖/禁用/关闭期间未呈现的对象移出锁域再析构。
  退休后重新取得 completion 锁推进现有 UNREF/detach 机制，不能让最后一个 pin 的释放等到无关 IRQ。
  临时升级的 page-flip event OFD 引用也必须移到锁外释放；最后一个 DrmFile.Drop 会重取 completion 锁，锁内释放会自锁。
  close 撤销查询可见的 active 后，实际引用由 ClosedScanout reset intent 保活；adapter 接受 disable 后再移交 pending disable，完成后锁外退休。
  已有 disable 在途时接管同一引用，不再重复排 reset；关闭期间完成的新 scanout 替换 intent 的旧目标，不能在 disable 前丢掉刚被硬件选中的对象。
  后继 master 的替换已接受时，旧 owner close 把原引用移交 pending.retiring，不能 reset 新 owner 的输出。
  非扫描配置直接锁外退休；在途非扫描 Commit 已承担 disable，不重复 reset。namespace 摘除与提交复核串行，最终 backing/模式释放仍在锁外。
  Framebuffer 引用不反向持有 DrmFile，不形成 OFD/framebuffer 环。

## GPU resource identity

- adapter 独占 host resource namespace 的保留前缀；启动、alternate scanout、光标与崩溃画面的角色由同一枚举分配。
  GraphicsDevice::first_dynamic_resource_id 将边界交给 DRM，后者的动态 allocator 必须从该值开始，不能复制常量。
  KMS framebuffer ID、GEM handle、context ID 属于各自命名空间，不得由数值相等推断与 host resource ID 相同。
- response mismatch 在 adapter fail-stop 前记录 opcode、context、首参数、响应码、长度和 exact fence；错误记录不放宽响应验证或继续复用失败队列。

## VirGL、PRIME 与隐式同步

`drm::graphics` 拥有资源及提交事务；`graphics::attachment` 拥有 file/context attachment，
`graphics::retirement` 分别退休 attachment、context 与设备级资源。`drm::prime` 拥有 dma-buf backend；
`device` 只在唯一 completion owner 下推进标准 VirtIO 命令。

- VirglBuffer 与导出 DrmFile 无关；dma-buf、KMS、GEM VMA、attachment 和提交 pins 分别保活它，禁止 dma-buf 反向持有 OFD/context。
  VirglLifecycle 的 mutex 只保护最后 fence 和导出 backend 的 Weak；后者保证重复导出同一 dma-buf identity，且不形成自环。
  DeviceMappingSource 保活实际 OFD、backing 与 MemoryObject；设备 VMA 的 split/fork 共享这些 owner，
  native VMAR 撤销映射并完成 TLB 退休后才释放。GEM 与 dumb backing 使用匿名 VMO；
  跨服务内存统计的当前边界由[内存 ABI](../syscall-support/memory.md)维护，不以 Linux shmem 分类冒充原生 VMO 统计。
  GEM VMA 与 PRIME 共用 DmaBuf 资源 owner，最后映射释放后主动推进退休，不依赖下一次无关 GPU 提交。
  只有组合独立设备对象与 OFD 的 mmap 准备阶段分配一次 lease；OOM 返回 ENOMEM 且不修改页表，unmap/fork 不新增 lease 分配。
  GEM_CLOSE 只撤销 handle/context attachment，不能提前 UNREF 仍有 VMA 的资源；仅保留物理 backing 不满足此契约。
- 每个 VirglAttachment 在 host attach 前预分配退休节点；接受命令立即记录 exact fence。
  close/失败/中断先移除 file-local handle，最后 pin 退出后等待 fence，发送 CTX_DETACH_RESOURCE；
  context 只等待自己的 detach，不等待其他进程仍持有的资源。全局退休观察者是最后一个 Arc 且 fence 完成后才 RESOURCE_UNREF。
- context_creation task mutex 串行化 context 创建、PRIME import 与 GEM_CLOSE 的 handle 发布，等待 host 时不持有 spin lock。
  重复/self-import 返回原 handle，不增 GEM handle 引用计数；未发布 handle 的 copyout/分配/等待失败统一回滚。
- 每资源 reservation task mutex 在 exec/transfer 的提交与 fence 记录间独占；多资源按设备 resource ID 排序获取，
  可中断获取返回 EINTR。对照 Linux virtio-gpu，exec/transfer 不在 CPU 上等待旧 fence：主机按控制队列顺序执行，
  先提交的生产者命令必然先于后提交的消费者命令生效；需要 CPU 可见结果的一方经 VIRTGPU_WAIT 或 DMA_BUF BEGIN_SYNC 等待。
  CPU 等待会把每次提交量化到主机 fence 通知周期（macOS QEMU 实测约 10 ms），合成每帧都被拖慢一个周期。
  VirglAccess 去重并保活真实 attachment，最后 fence 只取 max；独立资源不共享这把锁。
- EXECBUFFER 命令流上限 `VIRGL_COMMAND_MAX` 等于 Mesa virgl 命令缓冲最大值（64 Ki + 1024 dwords）。
  ioctl 在任何提交锁之前把命令流分段拷入匿名 VMO 的 DeviceBacking，`Submit3d` 以 Arc 持有它。
  adapter 按实际 BTI 区段构造 scatter/gather chain，不把物理碎片数量作为应用限制；请求头、响应各占一个 descriptor。
  若整条 chain 超过已协商的 controlq 容量，只有 GPU adapter 用自己的 BTI 分配连续 DMA backing 并复制命令，
  再进入原有唯一 publication/fence 路径；linuxd 不取得硬件分配能力。其余提交不新增分配或复制。
  整理时只复制已验证的 command 前缀：源在调用期间不可变，目标是独立新 VMO 的独占映射，
  两份映射均覆盖该长度且互不重叠；目标完成复制与 BTI 固定前不得交给设备。
  最终 backing/PMT 随槽位保活到 used-ring 归还；准备失败尚未发布，临时 VMO/映射/PMT 由 RAII 释放。
  这避免了队列全空仍无法提交的永久重试，也保证最大合法 EXECBUFFER 不因物理页离散而失败。
  大命令逐像素回读覆盖此边界；新增的条件复制由原有 600 帧 GPU 延迟门禁验证成本。
- dma-buf 支持标准 PRIME FD、DMA_BUF_IOCTL_SYNC、poll 与共享 backing mmap（当前 offset=0）。
  BEGIN_SYNC 等待同一 reservation/fence；END_SYNC 使用当前一致性 DMA backing。poll generation 来自资源 fence，不能被其他资源 completion 制造 ET 边沿。
  DeviceFile 可提供弱 OFD identity；syscall 负责 reserve FD、复用实际 OFD、copyout 后发布，CLOEXEC 属于 FD。
  候选 OFD 在锁外创建；缓存只持 Weak，关闭最后 FD 不形成 OFD/backend 环。
- renderD128 不取得 DRM master，禁止 KMS 与 SET/DROP_MASTER；primary 继续独占扫描输出。
  fs::GraphicsSysfsSource 由 DRM 注入只读实际 MMIO identity，sysfs 仅投影 card/render、设备号与 subsystem/uevent，fs 不依赖 driver。
- capset 按实际版本分别读取，不把最高版本 payload 当作 v0。virtio_gpu DRM version=0.0，不宣称未实现的 sync_file FD。
  非 blob legacy transfer 的 stride/layer_stride 必须为零；resource_create 的 size/stride 分别为 Linux u32 字段。
  VirGL 扫描面按 host 纹理的二维布局、格式和尺寸验证。其 GEM 大小只限制 CPU 映射与 transfer；
  与 Linux virtgpu 一致，`RESOURCE_CREATE` 的 `size=0` 分配一个来宾页。
  合成器保留两张原分辨率扫描纹理，通过 VirGL2 双向 `COPY_TRANSFER3D` 使用独立暂存。
  启动画面只占用一块暂存，首次桌面合成时释放；RFB 按请求矩形扩充暂存，由连接持有到断开。
  暂存和扫描纹理都登记提交 fence；启动画面等前次 flip 完成才重写，截图等暂存 fence 完成才读。
- DRM `DeviceFile::write_file_information` 在现有 GEM namespace 锁内只捕获数值、锁外格式化标准
  `drm-driver`、`drm-client-id`、`drm-total-memory`、`drm-shared-memory`、`drm-resident-memory`。client-id 来自唯一设备的
  monotonic OFD identity，dup/fork 共用；handle 关闭后不再归属该客户端，残留 VMA/KMS/退休 pins 仍按原寿命保活。
  当前 backing 全部页对齐且常驻系统内存，不冒充宿主专用显存。每客户端各 buffer 只计一次，跨客户端共享不可直接求和；
  不以 Arc 引用计数推断 GEM 共享，也不把 transport fence 等待时间发布为 GPU execution time。
  共享按 Linux `drm_gem_object_is_shared_for_memory_stats`（`handle_count > 1 || obj->dma_buf`）：VirGL 资源的
  `VirglLifecycle::shared` 在首次 PRIME 导出时置位，与 Linux 保留 `obj->dma_buf` 至最后一个 handle 关闭的寿命等价；
  dumb buffer 不可导出，恒为私有。fdinfo 在 OFD 锁内读取 lifecycle 锁，锁序与 `virgl_wait` 相同。
  统计沿用现有 owner，无新增状态、锁或分配；每次读取按客户端 GEM handle 数线性遍历，属于显式监控查询冷路径。
- completion 将退休节点移出锁域再析构；回收不分配。已发布 resource ID 不复用，未发布 ID 等 UNREF completion 后回收。
- 同步 fence、光标与队列准入等待先捕获设备通知 Pipe 的读 generation，再检查各自的完成条件。
  注册与唤醒以 generation 为准，其他等待者排空合并 token 仍可观察到完成。设备通知唤醒所有命中该 generation 的等待者，
  普通数据 Pipe 的读写继续按 level 与独占登记裁决。复用原有 generation、Pipe 和 wait registry，不增加队列或锁。
- Benchmark 决策：命令流改为每次提交分配 backing（与 Linux memdup 同量级），换掉 32 个 64 KiB 预分配槽位与锁内整段拷贝；
  去掉 exec/transfer 的 CPU fence 等待后，合成帧与 SHM 提交不再各自付出一个主机 fence 通知周期。
  新增 reservation 成本属于共享资源同步的必要契约；沿用 GPU 门禁检查真实清屏读回、PRIME 寿命、上游 Wayland EGL 消费与 page-flip 节拍，
  不以 host 墙钟推断 GPU 性能。具体结果只由实际运行记录声明。

当前只在同一设备共享 VirGL GEM；不宣称跨设备 PRIME、VirtIO-GPU resource blob、sync_file/syncobj、Vulkan 或全部 OpenGL resource transfer 语义。

primary OFD 关闭的 reset_after_owner 由同一 completion owner 保留到 disable 被 adapter 接受；
WouldBlock 不清除 intent、不 panic，新 master 在此期间不能提交新 scanout。
render/adapter/display completion 都先推进 reset，再推进 detach/UNREF，避免迟到的 disable 覆盖新所有者或扫描已释放资源。
首个合法 EXECBUFFER 与 RESOURCE_CREATE 一样可惰性发布 context；已发布 context 在 OFD 存活期间不可撤销，热路径不重复取得创建 task mutex。
未置位的 fence/ring 字段及零数量 syncobj 的 stride/pointer 按固定 Linux 语义忽略，不要求客户端写入私有 sentinel。

