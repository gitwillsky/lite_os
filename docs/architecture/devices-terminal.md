# 设备与终端当前架构

## 当前设计

1. 内核 `platform` 只发现 machine facts、路由中断并发布受权限约束的 MMIO/IRQ resource；
   不装配设备驱动。`devmgr::platform` 解码 DTB，`devmgr::drivers` 装配 VirtIO、PCI 与 xHCI。
2. devmgr 拥有全部硬件 adapter、queue、DMA、descriptor、completion 与 reset state。
   `device-model` 定义设备类接口；`device-protocol` 通过目录、channel 与 VMO 发布设备，
   linuxd 的 `devices::directory` 接入这些设备并建立 Linux 设备语义。netd 消费网络设备，ext4d 消费块设备。
3. 内核只把中断交给 Interrupt 对象。devmgr 的中断服务线程等待、确认设备再 ack；
   独立完成线程消费合并通知并分派 completion。请求线程睡在同一完成 owner 上，
   启动期也使用服务线程，不保留内核 bootstrap WFI 或 user-return deferred 驱动路径。
4. VirtIO ring 与固定 payload 用 VMO 分配，以对应 BTI 固定并持有 PMT；descriptor 使用设备地址，
   不能假定它等于 CPU 虚拟地址或物理地址。提交前预留完整 segment mapping，稳态只投影已缓存范围。
   reset 读回完成后才退休 descriptor、PMT 与 backing；设备仍可 DMA 时不得释放固定页。
5. `kernel::iommu` 独占 domain、IOVA 分配与设备页表，静态 `arch::dma` 后端编码 SMMUv3/RISC-V IOMMU。
   撤销或部分映射失败均先完成设备侧失效再回收表页和 pin；具体寿命契约见设备契约。
6. VirtIO block 与 Console 都由同一个 adapter 支持 modern MMIO/PCI；Console 按 exact port name 选择
   `com.redhat.spice.0`；port N 使用标准 data queue 编号，不能固定假设 host 分配 port 1。
   split virtqueue 的 used capability 必须先验证 identity、generation 与长度，再 exactly once 回收。
   非法 completion 关闭 adapter 并 reset，不能修补 free list 后继续运行。
7. QEMU `virt` 的 DMA 必须 coherent；不提供 non-coherent shadow buffer 或运行时 cache fallback。

- DRM owner 组合 display operation、GEM/framebuffer、KMS、damage fence、master 与 event，并经 `dri/card0` 设备文件拥有 Linux DRM UAPI。
  VirtIO-GPU config change 更新唯一 connector preferred mode，并只通过标准 DRM kobject hotplug
  uevent 通知 userspace；preferred mode 与已完成的 active CRTC mode 独立，旧 CRTC 在 userspace
  modeset 新 generation 前仍可 page-flip。同步 DRM ioctl 遇到 adapter 内部 display-info transaction
  时等待统一 readiness edge 后重提，不把 controlq 的实现级串行化泄漏成 `EBUSY`。动态 mode 的
  userspace transaction 由用户态 `compositor` 拥有：它是唯一 DRM master 与 evdev 读者，接管 devmgr 启动画面，
  以硬件光标跟随输入，并作为 Wayland 服务端把客户端窗口整帧合成到 scanout（`$XDG_RUNTIME_DIR/wayland-0`；
  `wl_seat` 有指针与键盘，特权连接有 layer-shell，所有连接有 foreign-toplevel）。键盘 keymap 由 `keyboard::xkb` 从同一张键码表生成，
  自包含、不依赖客户端的 xkeyboard-config。
  桌面继续使用 `wl_shm`；具备 VirGL2 的设备使用纹理合成与两块 GPU render target，纯 2D 设备使用 CPU/dumb buffer。
  Content 持有 SHM 像素（GPU 后端直接位于纹理）或 dma-buf 使用期；GPU 在途帧将 dma-buf 协议引用保活到 page-flip 完成。
  VirGL2 初始化成功才公布 linux-dmabuf v4，ARGB/XRGB 单平面隐式 modifier 直接 PRIME 导入，无 CPU 像素副本。
  GPU 路径已接入代码，尚未执行画面/性能验收。SHM 的 `damage`/`damage_buffer` 决定重读与上传的矩形，合成仍整帧重画；
  dma-buf 内容直接取样，不代表完整 OpenGL 兼容。
  Legacy `CURSOR2` 把 64×64 ARGB dumb BO 通过规范要求的
  `RESOURCE_CREATE_2D`/`TRANSFER_TO_HOST_2D` 投影到 VirtIO-GPU 独立 cursorq；该 queue 的
  completion 水位与 controlq fence 分离。纯 position update 异步发布，slot 忙时只保留尚未发布的最新
  坐标，并继承 cursorq 当前 resource 可见性；shape update 才等待 exact completion，因此 pointer motion
  不受 scene render、vblank 或旧坐标 completion 排队影响。
- input owner 组合 device state、每-open evdev queue、grab、clock 与 revoke，并经 `input/eventN` 设备文件拥有 evdev UAPI；VirtIO input adapter 只提供 raw event/config。
- 字符设备采用 Linux cdev/devtmpfs 模型：设备子系统向 `fs::chardev`/`fs::devnode` 注册 minor 区间 driver 与 devfs 节点，
  open 按 inode 设备号找到 driver，返回的 `DeviceFile` 拥有读写、poll、ioctl 与 mmap；fs、devfs 与 syscall
  不认识具体设备。mem（`null`/`zero`/`random`/`urandom`/`kmsg`）与 TTY（`devices::tty`/`console`/`ptmx`/`pts/N`）
  分别由 fs 与 `devices::tty` 自身注册；pts 设备号为 Linux `(136, index)`。
- 系统 console Terminal 是 `devices::tty` 持有的单例，`/dev/console`、init 的 fd 0/1/2 与 linuxd 串口接收线程共享它；
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

- 标准客户端协议：`wl_subcompositor` v1（同步缓存随父提交原子应用，位置与堆叠属于父状态）、
  `wp_viewporter` v1（逻辑尺寸与仿射采样元数据，CPU/GPU 直接取样，不预先生成缩放副本）、
  `zxdg_output_manager_v1` v3（逻辑尺寸=物理尺寸/scale）、`xdg_popup` 与 `xdg_positioner`（位置由锚点矩形、锚点、重力与偏移计算，相对父 surface 左上角，
  不使用窗口几何、不做 `constraint_adjustment` 越界调整；`grab` 的弹出层在点击外部时收到 `popup_done`）。
  未实现：`zwp_pointer_constraints_v1`、`zwp_relative_pointer_manager_v1`、`zwp_text_input_manager_v3`、`wp_fractional_scale_manager_v1`、
  `xdg_toplevel_icon_v1`、`zwlr_data_control_manager_v1`；全屏缩放的等比与黑边策略尚未设计（现在窗口按视口给出的尺寸整数倍放大）。
- 所有连接有 `wp_cursor_shape_manager_v1` v2（只支持 `wl_pointer`）：客户端按语义请求形状，compositor 用自己的光标集映射绘制；窗口边框上的调整大小箭头由 `classic-ui` 装饰按此请求。
- Wayland 外壳支持 layer-shell v5 的背景/任务栏/覆盖层、锚点、边距、独占区与键盘交互，以及 foreign-toplevel v3 的窗口列表和控制。
  `exclusive_zone=0` 的 layer 相对工作区（输出扣除其他图层的正独占区）定位，`-1` 与正值相对整个输出，所以外壳对话框
  （如“运行”：Top 层、锚定左下角）自然落在任务栏之上；按下指针按钮期间指针焦点固定在按下时的 surface（隐式抓取），
  拖动随指针移动的 layer 才不会丢失 motion 与释放。
  当前限制：layer popup 不支持；layer 的 configure 尺寸仍相对整个输出，
  多个同边 panel 尚不按独占区依次排列；foreign-toplevel 尚无 output_enter/leave 与父子窗口关系；
  硬件光标只显示放大后不超过 64×64、物理热点位于 0..63 的图像，其余回退默认箭头，不提供软件 cursor plane；
  未实现素材忙碌光标计时动画。
- BusyBox init 监督网络服务、显示服务与 UART shell；compositor 接续启动画面后进入 `logon`，认证成功启动用户 `desktop`；UART 保留恢复 shell，没有显示器文本 VT。
- GPU 通过 renderD128 开放每 OFD VirGL2 context、PRIME 导入/导出和隐式 fence；compositor 独占 primary/KMS。
  支持 3D resource/transfer/submit、VirGL scanout/flush、2D damage flush、
  64×64 hardware cursor 与
  单 connector mode change；Vulkan、跨设备 PRIME、显式 sync_file/syncobj、DRM atomic/auth/lease、完整 evdev
  output/multitouch 和设备热拔插尚未开放。
- VirtIO Console 只开放一个固定 SPICE agent named port，不提供 guest console、任意动态 port、PCI
  bridge/hotplug 或多显示器管理。
