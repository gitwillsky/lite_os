# 经典桌面应用框架方案（实施中）

Status: Active

阶段：用户已确认，开始实施；进度与未完成范围在本计划中维护。

目标：开发者提供窗口规格、控件布局、业务模型和命令处理；框架负责窗口生命周期、交互和渲染调度。
沿用 classic-ui，逐步迁移现有应用。跨进程继续使用固定版本的标准 Wayland 与 Linux UAPI。

实施进度：共享 ABI 运行库、安全 Rust SDK、Future/定时器/FD/剪贴板调度、订阅与 WindowHost 已落地。
七个普通系统应用与终端已迁移到共享控件 SDK；标准控件、菜单、属性表和通用文件/消息对话框使用同一 Tree。
列表的四种视图、多选、树展开、原地重命名、文字裁剪和交互失效由运行库拥有，应用只保留业务状态与副作用。
文件对话框提供共同类型筛选；后台 I/O、时区模型与设置文件的跨进程更新已收敛到各自 owner。
VirGL2 服务端合成已接入，纹理随不可变 Content 保活；移动只更新几何，Scanout 只分配选中后端的一对输出目标。
标准 GPU 客户端缓冲已接入代码，支持边界与资源/同步 owner 见 UI/设备契约；本机开发闭环、旧 ELF 升级和源码调试见 SDK 计划。
当前事实由[框架架构](../architecture/ui-framework.md)维护；本轮不执行测试/GUI 验收，编译检查不等同于运行验收。

## 实施前的重复逻辑

classic-ui 已有 Display、Decoration、Painter、Edit、FocusChain、Scroll、ColumnHeader、MenuBar 和 FileDialogWindow。
绘制原语较完整，但应用仍重复组合字体/scale、装饰事件、像素缓冲、重绘失效、frame 节流、提交和销毁。
六个应用和共享文件对话框出现相同的在途帧丢失重绘，说明这段生命周期应有共同 owner。
按钮绘制复用后，按下/释放、hover、焦点、键盘激活和失效通知仍散落在应用；布局、绘制与命中也常各算一遍。

## 分层与边界

| 层 | 提供的能力 | 应用承担的内容 |
|---|---|---|
| Kernel / Linux UAPI | 进程、文件、权限、poll/timer、DRM/KMS/GEM、evdev、音频 | 通过 std 或安全 typed library 使用 |
| Wayland compositor（系统用户态服务） | surface 提交、输入路由、窗口策略、合成、输出、缓冲同步与释放 | 遵守公开协议；第三方 toolkit 可直接接入 |
| classic-ui 应用运行时 | 事件循环、多窗口分发、定时任务、窗口装饰、资源与重绘调度 | 创建窗口、注册业务事件 |
| 标准控件、布局与对话框 | 排版、裁剪、焦点、交互、滚动、菜单、属性页、文件对话框 | 提供数据、控件规格与业务命令 |
| 应用 | 文档、进程列表、播放状态、设置校验等业务模型 | 自有模型、命令、副作用及必要的自绘内容 |

借鉴 Windows 2000 的 USER32 窗口/消息职责、GDI 绘图职责、Comctl32 标准控件和通用对话框/属性表职责。
这里采用这些能力边界；LiteOS 的 UI 框架和合成器继续在用户态。方案不等同于实现完整 Win32 ABI。
现有 Rust dylib 的同工具链约束仅适用于当前系统组件，不作为第三方应用 SDK 的长期约束。
公开 SDK 使用安全 Rust API，内部维护稳定二进制边界和版本化共享控件运行库，先完成
[SDK 与系统内开发设计](classic-ui-sdk.md)，再扩展框架公共接口。

## 优先收敛的共同 owner

| Owner（名称待实现时确定） | 唯一状态与责任 |
|---|---|
| Display（保留） | Wayland 连接、对象、权威 configure、在途 callback、buffer.release；其他层只查询，不镜像同步 |
| Application | 一个进程的事件循环、窗口路由、定时/可读事件、退出和错误清理 |
| WindowHost | 每个窗口的 Decoration、绘图存储、未消费失效和默认事件处理；不复制 Display 的协议状态 |
| UI context resources | 主题、字体和图标的生命周期与变更通知；按实际需要复用，不预先增加全局字形缓存 |
| 控件 | 自身 hover/armed/focus、编辑/选择/滚动状态；产生业务 action 和失效请求 |
| 布局容器 | 控件最终逻辑矩形；绘制、裁剪、滚动及 hit-test 消费同一份排版结果 |
| 应用模型 | 文件内容、系统采样、播放任务等领域事实；控件内未提交的交互草稿不代替持久业务状态 |

第一阶段先统一窗口级失效，保留当前整窗绘制语义；稳定后再实现局部 invalidation。
请求在 frame 未完成时累积，只有成功提交后消费；callback 控制绘制节奏，release 控制缓冲可写性，二者独立。
空闲时等待真实事件和最近的业务 deadline；时钟/进程采样仍有自己的定时任务，不用统一高频轮询刷新所有窗口。
后台文件操作或播放通过消息/可读事件回到 UI 线程，不从工作线程修改控件或 Wayland 对象。
时区读取/重载已改为固定 tz-rs 的标准 TZif 解析，不修改进程环境，允许继续扩展后台任务。

控件行为与上述运行时进入共享库，SDK 不静态复制这些实现；资源升级与旧进程存活遵循 SDK 设计。
断连以错误交给应用处理保存和退出，运行库不直接终止进程；关闭窗口不提前复用尚未 release 的协议缓冲。

## 事件、帧与生命周期契约

1. 一次事件分发完成后再处理下一次；模态对话框使用事件状态，不嵌套运行事件循环。
   应用回调不在持锁、绘制或控件可变借用期间执行；回调内关闭采用延迟销毁，迟到的后台结果校验代际。
2. 事件循环同时等待 Wayland、后台任务 FD 和最近 deadline；队列有容量与背压，UI 线程不等待依赖 UI 的工作线程。
   现有剪贴板已经使用标准 wl_data_device；继续复用，阻塞式读取改为事件循环驱动并覆盖取消、断连和传输上限。
3. wl_surface.frame 仅表示适合绘制下一帧的提示，不能视为实际显示完成；现有 FramePresented 命名在公共 API 冻结前纠正。
   隐藏窗口可能没有 callback，不能因此暂停输入、I/O 或退出清理；真实呈现测量使用标准 presentation-time。
4. callback 与 buffer.release 各自推进；可复用缓冲池有上限，无空闲缓冲时保留失效并返回事件循环，不覆盖在途内容。
   局部重绘追踪每个缓冲的内容历史，覆盖移动前后区域、遮挡、透明与 scale；先保持整窗绘制正确再增量优化。

上述标准能力均先核对固定协议、服务端处理与客户端调用；能力缺失时按标准补齐并记录实际支持范围。
frame 的含义以 [Wayland wl_surface.frame](https://wayland.freedesktop.org/docs/html/apa.html#protocol-spec-wl_surface-request-frame) 为准。

## Rust 编程模型与异步执行

应用接口优先使用所有权与 RAII 管理窗口、订阅和任务；newtype 区分对象标识与坐标单位，
enum 表达事件和业务动作，Result 支持正常的错误传播。trait/generic 用于应用侧扩展与类型检查，
不把 Rust 对象布局变成公共 ABI，也不要求开发者手写回调 userdata 或裸指针。

1. 应用用 async/await 等待文件操作、剪贴板传输、定时器、对话框结果和后台任务。
   UI Future 可为非 Send，固定在 UI 线程轮询；跨线程工作只接收满足 Send 的自有数据并返回结果。
   窗口和控件句柄保持线程亲和性；跨 await 不保留控件可变借用或锁，结果回来后重新校验目标是否仍存在。
2. Application 仍拥有唯一事件循环与 I/O 等待；SDK 的 Future 调度通过同一循环接入，不另起竞争的 Wayland reader。
   Waker 只登记就绪并唤醒循环，不在 wake 中立即执行控件代码；跨线程 wake 必须线程安全，任务登记与再次检查避免丢失唤醒。
   Pending 任务等实际通知，不按固定间隔扫描；就绪任务分批轮询，为输入和绘制保留调度机会。
   单次 poll 必须很快返回；调度预算只能在 poll 之间生效，不能抢占一段长时间计算。
3. 普通同步文件 I/O 和 CPU 密集任务进入有界后台执行，不能仅包一层 async 后在 UI 线程阻塞。
   socket/pipe 等非阻塞 I/O 使用 readiness；磁盘异步路径按已验证的标准 UAPI 单独评估，不能把普通文件 poll 就绪当成 I/O 已完成。
   运行时选型先核对所需 syscall、线程与定时器；优先复用成熟实现，不默认完整 Tokio 已能运行，也不预先自建通用异步生态。
4. 窗口关闭撤销其作用域任务、订阅和未完成请求；不默认放任任务脱离窗口存活。
   Future 被丢弃时解除订阅并申请取消，底层仍需完成资源清理；已写入的数据或已提交的副作用不会被自动撤销。
   必须等待的 flush/完成确认用显式异步关闭表达，Drop 不阻塞事件循环；迟到完成不得访问已释放的回调或控件。

这些约束遵循 [Future 的非阻塞 poll 契约](https://doc.rust-lang.org/std/future/trait.Future.html)与
[Waker 的跨线程唤醒语义](https://doc.rust-lang.org/std/task/struct.Waker.html)。
最小切片验证等待期间仍处理输入/重绘、并发任务公平性、关闭时取消、唤醒竞争与空闲无忙轮询；
测量就绪任务调度成本、唤醒次数及长任务对帧提交的影响，不以 async 语法本身作为性能证明。

## 控件和开发方式

标准控件持有交互状态：Button、Edit、CheckBox/Radio、ListView/Header、ScrollBar、Tab、Menu、StatusBar。
列表按可见行绘制，应用提供稳定行标识和数据；进程列表不为每行创建独立窗口或复杂控件树。
布局先提供常用行列、停靠、间距和内容度量；文字统一选择单行省略或段落换行，禁止各应用另写截断和裁剪。
布局结果统一逻辑坐标、缩放舍入和最小尺寸溢出策略；绘制、裁剪与命中消费同一结果。
物理按键、快捷键和已提交 Unicode 文本分别处理；输入法按标准协议核对接入，不用私有按键消息替代。
PropertySheet 统一标签页、内容区域、OK/Cancel/Apply、焦点与错误展示；每页只负责设置数据、校验和应用。
Apply 先完成全部页面校验，再执行各领域更新；Cancel 丢弃尚未应用的草稿，不承诺跨系统时钟与文件的原子回滚。
通用文件对话框复用相同 Tree、WindowHost 和事件路由，父子关系与模态提示按固定标准协议核对接入。
xdg_toplevel.set_parent 只建立父子关系，不单独作为模态策略；嵌套事件循环不作为模态实现。
图表、文本编辑画布等特殊内容继续允许 Painter 自绘，并接受统一布局、裁剪和失效调度。
控件同时提供语义角色、键盘遍历和焦点信息；辅助技术桥接另列标准与支持矩阵，不把绘制完成视为已支持无障碍。

开发一个应用所需的主要输入变为：窗口规格 → 布局/控件 → 数据模型 → 命令与更新处理。
应用代码中不再出现字体目录、frame callback、ready_to_draw、memfd、像素提交和标准装饰分发。
这些是进程内库 API，不增加跨进程私有消息或私有 Wayland 绘图指令。

## CPU / GPU 路线

1. 应用运行时与业务代码不选择合成器后端；CPU/GPU 使用相同的标准 surface、commit、damage 和同步语义。
2. 优先评估现有 VirGL2 做服务端 GPU 合成：继续接收标准 wl_shm，提交内容持有纹理，移动只更新几何。
   Content/已提交缓冲拥有资源生命周期，不在 Session 另建一份需要手动同步的可写内容表。
3. 已接入 linux-dmabuf v4、DRM PRIME/dma-buf、render node 和共享资源隐式 fence。
   GEM 资源独立于导出 OFD/context，attachment 先 detach、context 独立销毁；重复/self-import 合并 handle。
   上游 Mesa 25.1.9 提供 EGL/GBM/Wayland 客户端；当前限定同设备单平面 ARGB/XRGB 隐式 modifier。
   无界面标准消费者、共享寿命与显示节拍分别验收；完整 OpenGL/Vulkan 及更多格式仍不能由此推断。
4. CPU 局部合成需覆盖旧/新窗口区域、遮挡、scale 和双缓冲历史；不能只在当前 back buffer 擦一个矩形。

Wayland 规定应用与合成器的缓冲交互，不规定某种控件库或 CPU/GPU 绘制算法。
本轮原生微基准说明存在合成和传输成本，尚未证明 CPU 是 guest 整条链路最大的瓶颈。
GPU 方案的验收应分别记录上传、合成、提交与显示完成；保留标准协议消费者验证，主观 UI 效果由用户验收。

## 迁移顺序 → 验证点

前置：明确公开 SDK ABI、版本与内存契约，并打通系统内编译/链接/运行的最小闭环。
→ 应用可独立于 OS 工具链构建；旧二进制不重编即可消费兼容更新后的运行库。

1. 运行时与 WindowHost，先迁移一个普通窗口和一个文件对话框。
   → configure、在途失效、回调内关闭、隐藏恢复、断连回收、多窗口和上述异步契约正确；无变化零重绘。
2. 标准控件与布局，迁移任务管理器和资源管理器列表。
   → 排版/命中同源、键盘/滚轮一致、大列表仅处理可见行、同类溢出通过共同控件修复。
3. PropertySheet 与通用对话框，迁移控制面板及其调用方。
   → 应用只保留数据和动作，按钮/标签页/页内容的生命周期统一，重复实现随迁移删除。
4. GPU 合成与后续 GPU 客户端缓冲能力按测量推进。
   → 原有标准消费者保持可用，提交/释放/同步正确，并以同一 workload 对照成本。

每阶段完成后迁移下一批，不保留两套产品运行时，不先批量改造所有应用。
回归聚焦实际生产契约和故障模式，不为简单脚本或薄包装机械增加测试；阶段完成时再跑相关范围。
按上述顺序实施；本轮依用户要求不维护测试用例、不执行测试，未验证能力不声明已通过验收。

## 一手参考

- [Wayland 架构](https://wayland.freedesktop.org/docs/book/Architecture.html)
- [Windows 消息与窗口过程](https://learn.microsoft.com/en-us/windows/win32/winmsg/about-messages-and-message-queues)
- [InvalidateRect：失效区域累积](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-invalidaterect)
- [Windows 标准控件](https://learn.microsoft.com/en-us/windows/win32/controls/window-controls)
- [Property Sheet：页面、公共按钮与通知](https://learn.microsoft.com/en-us/windows/win32/controls/property-sheets)
