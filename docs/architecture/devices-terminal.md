# 设备与终端当前架构

## 当前设计

- `platform` 发现并装配具体 adapter；`drivers` 只公开 block、network、display、input、PCM output、
  entropy、named byte-stream port 与 interrupt 等通用 seam。`drivers::registry` 为每个设备类保存一个
  按发现顺序编号、只追加的多实例注册表，注册只接受 trait 对象；选择实例是消费领域的策略：根文件
  系统、IPv4/AF_PACKET、DRM、ALSA、SPICE port 与 entropy 当前绑定 index 0，evdev 发布全部 input。
- VirtIO queue 与 DMA payload 由各 adapter 拥有；block/RNG 的 request slot、descriptor identity、
  lost-wake handshake 与 capacity wait 由 `drivers::io_completion` 统一拥有。hardirq 共用
  transport-error latch，只确认 MMIO 并发布 `DriverIo` deferred bit，不进入 ordinary adapter lock；使用该
  vector 的 adapter 构造成功时自报为 `CompletionSource`，completion 在统一
  user-return/idle safe point 消费；VirtIO-net RX 由单一 slot lifecycle owner 原子
  claim/repost/retire。
- split virtqueue 摘取 used entry 只产生一个 `UsedDescriptor` capability，不立即回收 chain；
  concrete adapter 必须先 claim slot/generation/head 并验证 device-written length/response，随后才
  exactly-once recycle。duplicate、unknown、非法 length 或 sequence completion 永久关闭 queue 并
  reset device。真实 VirtQueue 回归测试固定：单 descriptor completion 在 claim 前 free count 保持
  3/4，合法 recycle 后恢复 4/4，duplicate 保持 4/4、unknown/out-of-range 保持 3/4，均不得二次回收。
- net RX/TX、input、GPU control/damage、block request 与 RNG 固定 buffer 初始化时建立并持有
  `DmaBuffer` 物理 segment mapping；steady-state virtqueue submission 只投影 cached ranges，地址
  空间锁与 page walk 均为 0。adapter drop 写 reset 并等到 status 读回 0 后，再释放这些 mapping。
- QEMU `virt` backend 把 DTB `dma-coherent` 作为必需 machine fact；VirtIO 不维护 non-coherent shadow buffer 或运行时 cache-maintenance fallback。
- VirtIO Console adapter 只协商 modern multiport，固定选择 `com.redhat.spice.0`；同一个 transport-neutral adapter 在 UTM 产品路径消费 modern VirtIO PCI common/notify/ISR capabilities，在 headless gate 消费 modern MMIO，绝不复制 queue 或 port state。它拥有 control
  RX/TX、exact-name 命中后按 port id 选择的唯一 data RX/TX pair、named-port open state 与 64 KiB
  byte ring。UTM 可以先枚举 `org.qemu.guest_agent.0`，因此 data queue 不得固定为 port 1 的 4/5；
  port 0 使用 0/1，port N 使用 `2N+2`/`2N+3`。hardirq 只 ack 并发布 `virtio_port` 注册绑定的
  deferred vector；safe point 验证 completion、repost RX、通知 queue 并经 `virtio_port` domain
  唤醒 task。`virtio_port` 经字符设备注册表把该 stream 发布为
  `/dev/virtio-ports/com.redhat.spice.0`。
- GPU runtime completion 由独立 sequence owner 验证 fence/response 与 stage 顺序，阶段分支只选择
  下一条 `GpuCommand`；统一 command seam 负责 wire encoding、长度与 queue publication。
  DRM readiness generation 在同一次 completion lock 内前进。该层只增加 fixed enum dispatch 与整数
  increment，不增加 lock、allocation 或 descriptor 数；MMIO completion 主导实际 latency，因此不新增
  微基准，改由 architecture cost gate 固定 `poll_update` 的 direct assembly 与 direct publication
  为 0、sequence submission 出口至多 1 个。
- VirtIO-block 使用 16 个固定 DMA request slots，允许乱序 multi-outstanding；同步 caller 在
  task context 睡眠，bootstrap caller 以 trap-PC-resumed external IRQ/WFI 原子等待同一
  completion owner；第 17 个
  caller 进入 FIFO capacity wait，slot release 直接 handoff，不伪造设备故障。
- block completion 消费 used `len`：4 KiB Read 只接受 4097（data+status），Write/Flush 只接受 1
  （status）。短/超长 completion 在接触 status 或返回 read data 前 fail-stop reset，并由 request
  claim owner 的 reject→drain 路径 exactly once 完成和释放所有受影响 slot。
- VirtIO-RNG 使用 4 个固定、提交前不预零的 4 KiB device-write DMA slots；task caller 睡眠，
  创建 init `AT_RANDOM` 的唯一 cold-boot caller WFI。64 KiB `getrandom` 或 entropy-device read 从
  256 个 256-byte batch 降为 16 个 heap-backed 4 KiB batch；固定 64-poll 模型的 MMIO polling/
  spin 从 64/64 降为 0/0，output/DMA 覆盖前预零从 131072/4096 bytes 降为 0/0。
- DRM owner 组合 display operation、GEM/framebuffer、KMS、damage fence、master 与 event，并经 `dri/card0` 设备文件拥有 Linux DRM UAPI。
  VirtIO-GPU config change 更新唯一 connector preferred mode，并只通过标准 DRM kobject hotplug
  uevent 通知 userspace；preferred mode 与已完成的 active CRTC mode 独立，旧 CRTC 在 userspace
  modeset 新 generation 前仍可 page-flip。同步 DRM ioctl 遇到 adapter 内部 display-info transaction
  时等待统一 readiness edge 后重提，不把 controlq 的实现级串行化泄漏成 `EBUSY`。动态 mode 的
  userspace transaction 由用户态 `compositor` 拥有：它是唯一 DRM master 与 evdev 读者，接管内核启动画面，
  以硬件光标跟随输入，并作为 Wayland 服务端把客户端窗口整帧合成到 scanout（`$XDG_RUNTIME_DIR/wayland-0`；
  `wl_seat` 有指针与键盘，特权连接有 layer-shell，所有连接有 foreign-toplevel（同一桌面的程序可枚举与控制窗口，与 Windows 2000 一致）；尚无 popup、登录或桌面程序）。键盘 keymap 由 `keyboard::xkb` 从同一张键码表生成，
  自包含、不依赖客户端的 xkeyboard-config。
  Legacy `CURSOR2` 把 64×64 ARGB dumb BO 通过规范要求的
  `RESOURCE_CREATE_2D`/`TRANSFER_TO_HOST_2D` 投影到 VirtIO-GPU 独立 cursorq；该 queue 的
  completion 水位与 controlq fence 分离。纯 position update 异步发布，slot 忙时只保留尚未发布的最新
  坐标，并继承 cursorq 当前 resource 可见性；shape update 才等待 exact completion，因此 pointer motion
  不受 scene render、vblank 或旧坐标 completion 排队影响。
- input owner 组合 device state、每-open evdev queue、grab、clock 与 revoke，并经 `input/eventN` 设备文件拥有 evdev UAPI；VirtIO input adapter 只提供 raw event/config。
- 字符设备采用 Linux cdev/devtmpfs 模型：设备子系统向 `fs::chardev`/`fs::devnode` 注册 minor 区间 driver 与 devfs 节点，
  open 按 inode 设备号找到 driver，返回的 `DeviceFile` 拥有读写、poll、ioctl 与 mmap；fs、devfs 与 syscall
  不认识具体设备。mem（`null`/`zero`/`random`/`urandom`/`kmsg`）与 TTY（`tty`/`console`/`ptmx`/`pts/N`）
  分别由 fs 与 `tty` 自身注册；pts 设备号为 Linux `(136, index)`。
- 系统 console Terminal 是 `tty` 持有的单例，`/dev/console`、init 的 fd 0/1/2 与 deferred UART 输入共享它；
  `/dev/tty` 按调用者 controlling terminal 的设备号重开底层设备。PTY registry、pair 与 Terminal
  session/foreground/winsize 各守自己的 seam；job control 只经 task 安装的 `JobControl`；控制面使用标准
  PTY、termios、ANSI/ECMA-48。
- PTY index registry 使用单一 AVL owner；pair、Terminal、Pipe 和索引 node 都在自旋锁外准备，
  发布只重验候选索引并原子替换退休 weak slot；devpts 枚举也先在锁外准备容量再重验。
  actual master fput 发布 master closed、关闭输出 reader、唤醒两端并锁外交付一次 hangup；
  每个独立 slave open 的 admission 在 actual fput 归还一次。观察 backend 的 Arc 不延迟 hangup，
  也不阻止两端全部关闭后的 index 复用。失败准备不发布任何 slot 或 slave admission。
  此变更只影响 cold open/getdents，不增加 byte-stream 热路径成本；不新增独立性能阈值，
  由实际 owner 的 OOM、观察者、重开和重复 close 回归用例验证。
## Known limits

- 为 Wine（winewayland）补齐的协议：`wl_subcompositor` v1（子 surface 随根窗口绘制；位置在父提交时生效，`set_sync`/`set_desync` 不单独缓存内容——已记录的简化）、
  `wp_viewporter` v1（提交时按源矩形裁剪、最近邻缩放成目标尺寸×buffer scale，原始缓冲保存以便只改视口的提交重新取样）、
  `zxdg_output_manager_v1` v3（逻辑尺寸=物理尺寸/scale）、`xdg_popup` 与 `xdg_positioner`（位置由锚点矩形、锚点、重力与偏移计算，相对父 surface 左上角，
  不使用窗口几何、不做 `constraint_adjustment` 越界调整；`grab` 的弹出层在点击外部时收到 `popup_done`）。
  未实现：`zwp_pointer_constraints_v1`、`zwp_relative_pointer_manager_v1`、`zwp_text_input_manager_v3`、`wp_fractional_scale_manager_v1`、
  `xdg_toplevel_icon_v1`、`zwlr_data_control_manager_v1`（Wine 的可选项）；全屏缩放的等比与黑边策略尚未设计（现在窗口按视口给出的尺寸整数倍放大）。
- 所有连接有 `wp_cursor_shape_manager_v1` v2（只支持 `wl_pointer`）：客户端按语义请求形状，compositor 用自己的光标集映射绘制；窗口边框上的调整大小箭头由 `classic-ui` 装饰按此请求。
- Wayland 外壳支持 layer-shell v5 的背景/任务栏/覆盖层、锚点、边距、独占区与键盘交互，以及 foreign-toplevel v3 的窗口列表和控制。
  `exclusive_zone=0` 的 layer 相对工作区（输出扣除其他图层的正独占区）定位，`-1` 与正值相对整个输出，所以外壳对话框
  （如“运行”：Top 层、锚定左下角）自然落在任务栏之上；按下指针按钮期间指针焦点固定在按下时的 surface（隐式抓取），
  拖动随指针移动的 layer 才不会丢失 motion 与释放。
  当前限制：layer popup 不支持；layer 的 configure 尺寸仍相对整个输出，
  多个同边 panel 尚不按独占区依次排列；foreign-toplevel 尚无 output_enter/leave 与父子窗口关系；
  硬件光标只显示放大后不超过 64×64、物理热点位于 0..63 的图像，其余回退默认箭头，不提供软件 cursor plane；
  未实现素材忙碌光标计时动画、客户端 scale 高于输出时的缩小。
- BusyBox init 监督网络服务、显示服务与 UART shell；compositor 接续启动画面后进入 `logon`，认证成功启动用户 `desktop`；UART 保留恢复 shell，没有显示器文本 VT。
- GPU 开放 compositor 独占的单一 VirGL2 context、3D resource/transfer/submit、VirGL scanout/flush、2D damage flush、
  64×64 hardware cursor 与
  单 connector mode change；Vulkan、多 client 3D context、DRM atomic/auth/lease、完整 evdev
  output/multitouch 和设备热拔插尚未开放。
- VirtIO Console 只开放一个固定 SPICE agent named port，不提供 guest console、任意动态 port、PCI
  bridge/hotplug 或多显示器管理。
