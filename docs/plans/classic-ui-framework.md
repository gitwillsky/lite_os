# 经典桌面应用框架方案（待确认，尚未实施）

Status: Active

阶段：方案评审；等待用户确认，尚未开始实施。

目标：开发者提供窗口规格、控件布局、业务模型和命令处理；框架负责窗口生命周期、交互和渲染调度。
沿用 classic-ui，逐步迁移现有应用。跨进程继续使用固定版本的标准 Wayland 与 Linux UAPI。

## 当前缺口

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
公开 SDK 改以稳定 C ABI、语言绑定和版本化运行库为设计方向，先完成
[SDK 与系统内开发设计](classic-ui-sdk.md)，再扩展框架公共接口。

## 优先收敛的共同 owner

| Owner（名称待实现时确定） | 唯一状态与责任 |
|---|---|
| Display（保留） | Wayland 连接、对象、权威 configure、在途 callback、buffer.release；其他层只查询，不镜像同步 |
| Application | 一个进程的事件循环、窗口路由、定时/可读事件、退出和错误清理 |
| WindowHost | 每个窗口的生命周期、Decoration、可复用绘图存储、未消费失效和标准默认事件处理 |
| 进程级 UI resources | 主题、字体和图标的生命周期；按实际需要复用，不预先增加全局字形缓存 |
| 控件 | 自身 hover/armed/focus、编辑/选择/滚动状态；产生业务 action 和失效请求 |
| 布局容器 | 控件最终逻辑矩形；绘制、裁剪、滚动及 hit-test 消费同一份排版结果 |
| 应用模型 | 文件内容、系统采样、播放任务等领域事实，不保存第二份可修改控件状态 |

第一阶段先统一窗口级失效，保留当前整窗绘制语义；稳定后再实现局部 invalidation。
请求在 frame 未完成时累积，只有成功提交后消费；callback 控制绘制节奏，release 控制缓冲可写性，二者独立。
空闲时等待真实事件和最近的业务 deadline；时钟/进程采样仍有自己的定时任务，不用统一高频轮询刷新所有窗口。
后台文件操作或播放通过消息/可读事件回到 UI 线程，不从工作线程修改控件或 Wayland 对象。
时区变化订阅应消除现有运行时修改进程 TZ 的单线程限制，再扩展相关后台任务。

## 控件和开发方式

标准控件持有交互状态：Button、Edit、CheckBox/Radio、ListView/Header、ScrollBar、Tab、Menu、StatusBar。
列表按可见行绘制，应用提供稳定行标识和数据；进程列表不为每行创建独立窗口或复杂控件树。
布局先提供常用行列、停靠、间距和内容度量；文字统一选择单行省略或段落换行，禁止各应用另写截断和裁剪。
PropertySheet 统一标签页、内容区域、OK/Cancel/Apply、焦点与错误展示；每页只负责设置数据、校验和应用。
FileDialogWindow 复用相同 WindowHost 和事件路由，父子关系与模态提示按固定标准协议核对接入。
图表、文本编辑画布等特殊内容继续允许 Painter 自绘，并接受统一布局、裁剪和失效调度。

开发一个应用所需的主要输入变为：窗口规格 → 布局/控件 → 数据模型 → 命令与更新处理。
应用代码中不再出现字体目录、frame callback、ready_to_draw、memfd、像素提交和标准装饰分发。
这些是进程内库 API，不增加跨进程私有消息或私有 Wayland 绘图指令。

## CPU / GPU 路线

1. 应用运行时与业务代码不选择合成器后端；CPU/GPU 使用相同的标准 surface、commit、damage 和同步语义。
2. 优先评估现有 VirGL2 做服务端 GPU 合成：继续接收标准 wl_shm，提交内容持有纹理，移动只更新几何。
   Content/已提交缓冲拥有资源生命周期，不在 Session 另建一份需要手动同步的可写内容表。
3. GPU 客户端直接共享缓冲时补齐标准 linux-dmabuf、DRM PRIME/dma-buf 和相应同步能力；当前尚未实现。
   EGL/GBM/Mesa 接入另按真实 UAPI 需求核对，不能由现有 VirGL clear/submit 就推断外部 GPU 应用已兼容。
4. CPU 局部合成需覆盖旧/新窗口区域、遮挡、scale 和双缓冲历史；不能只在当前 back buffer 擦一个矩形。

Wayland 规定应用与合成器的缓冲交互，不规定某种控件库或 CPU/GPU 绘制算法。
本轮原生微基准说明存在合成和传输成本，尚未证明 CPU 是 guest 整条链路最大的瓶颈。
GPU 方案的验收应分别记录上传、合成、提交与显示完成；保留标准协议消费者验证，主观 UI 效果由用户验收。

## 迁移顺序 → 验证点

前置：明确公开 SDK ABI、版本与内存契约，并打通系统内编译/链接/运行的最小闭环。
→ 应用可独立于 OS 工具链构建；旧二进制不重编即可消费兼容更新后的运行库。

1. 运行时与 WindowHost，先迁移一个普通窗口和一个文件对话框。
   → 标准 configure 顺序、在途失效不丢、无变化零重绘、断连/关闭回收、定时和多窗口路由正确。
2. 标准控件与布局，迁移任务管理器和资源管理器列表。
   → 排版/命中同源、键盘/滚轮一致、大列表仅处理可见行、同类溢出通过共同控件修复。
3. PropertySheet 与通用对话框，迁移控制面板及其调用方。
   → 应用只保留数据和动作，按钮/标签页/页内容的生命周期统一，重复实现随迁移删除。
4. GPU 合成与后续 GPU 客户端缓冲能力按测量推进。
   → 原有标准消费者保持可用，提交/释放/同步正确，并以同一 workload 对照成本。

每阶段完成后迁移下一批，不保留两套产品运行时，不先批量改造所有应用。
回归聚焦实际生产契约和故障模式，不为简单脚本或薄包装机械增加测试；阶段完成时再跑相关范围。
框架公共 API、控件树和应用迁移均等待用户确认后实施。

## 一手参考

- [Wayland 架构](https://wayland.freedesktop.org/docs/book/Architecture.html)
- [Windows 消息与窗口过程](https://learn.microsoft.com/en-us/windows/win32/winmsg/about-messages-and-message-queues)
- [InvalidateRect：失效区域累积](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-invalidaterect)
- [Windows 标准控件](https://learn.microsoft.com/en-us/windows/win32/controls/window-controls)
- [Property Sheet：页面、公共按钮与通知](https://learn.microsoft.com/en-us/windows/win32/controls/property-sheets)
