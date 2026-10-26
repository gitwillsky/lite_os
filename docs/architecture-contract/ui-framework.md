# UI 框架与 SDK 契约

当前实现见[UI 架构](../architecture/ui-framework.md)；Wayland 固定版本由[规范基线](../standards-baseline.md)维护。

## Owner 与依赖

- linux-uapi 只拥有 Linux libc/UAPI mechanism；classic-ui 拥有标准 Wayland 客户端、绘制与 WindowHost。
- classic-ui-abi 是无运行期状态的 no_std 布局声明；classic-ui-runtime 依赖该声明与 classic-ui，实现共享控件。
- liteos-ui 只依赖 ABI 声明，提供安全 Rust 生命周期、事件、Future 与回调适配，不静态复制控件实现。
- Display 唯一拥有协议对象、configure、callback、缓冲与释放；WindowHost 不复制这些状态。
- Context 唯一拥有窗口表、控件树、资源与 FD interest；对象标识单调递增不复用，迟到事件不能命中新对象。

## ABI 与内存安全

1. 唯一动态入口是 liteos_ui_get_api；SDK 先读取版本/尺寸头，再构造完整函数表引用。
   函数表是只读静态值，没有可变 global 或按工具链分派的第二条入口。
2. context 只在创建线程使用；SDK 的 Rc owner 和非 Send/Sync 句柄禁止跨线程操作。
   所有 unsafe ABI 调用只临时借用 context，调用返回前不保留 Rust 借用，不嵌套执行应用回调。
3. 所有输入数组/UTF-8 在调用期间有效；运行库复制后才保存。输出缓冲必须独占且不与输入重叠。
   各模块释放自己的分配，String/Vec/Future/Waker/trait object 不能穿过 ABI。
4. 创建/销毁成对；SDK 最后一个 owner 销毁一次。父对象保活期间子对象有效；显式关闭父窗口先撤销子窗口。
   已关闭句柄返回错误，禁止重用对象编号或把无效整数强转 Rust enum。
5. panic 不跨 ABI 展开。运行库采用 abort profile；取消不是事务回滚，Drop 不等待后台线程。

SDK 只允许 connection.rs 导入 liteos_ui_get_api；ABI 声明与运行库导出没有 libc/设备 foreign import。
原始 OS/设备 FFI 仍只允许位于 linux-uapi，不能借公开 SDK 边界扩大到其他目录。

## 事件、等待与退出

- wl_surface.frame 与 wl_buffer.release 独立；buffer 忙时不覆盖，在途失效只在提交成功后消费。
- 外部 PollFd 带 fd owner 的类型借用；批量等待后只投影 readiness，不延长其他描述符的生命周期。
  PollBuffer 只持有不可自行轮询的 raw 存储；每轮 PollSet 重新建立有效借用，析构清空旧编号并保留容量。
  唯一切片转换位于 linux-uapi，依据 repr(transparent) 与 push 借用前置条件；不建立 static FD 或从旧轮次复活编号。
- Transport 独占输出片段及其 FD，非阻塞发送保留部分写偏移，首次正数 sendmsg 后移除 FD，后续仅发送余下字节。
  普通 Writer 尾部在带 FD 请求前入队，保持 wire/SCM_RIGHTS 顺序；上限错误是连接失败，不能重试重放 commit。
- Waker 的 AtomicBool 合并重复登记，Mutex 只保护短队列；不在锁内 poll Future 或调用应用代码。
  入队后写非阻塞 socket，满 socket 已有未消费通知；注销后的唤醒无 UI 对象访问。
- Task/窗口关闭后，Future 在 UI 线程释放；FD interest、定时器与接收端通过各自 RAII owner 撤销。
  在途 clipboard 传输和协议缓冲不依赖已释放的回调 userdata。
- 协议断开向应用返回错误；库不调用 process::exit。Application 析构取消任务、关闭窗口，再释放连接。
- 新热路径的测量项为缓冲复用、任务调度/唤醒、布局与可见行绘制；本轮按用户要求不运行测试或性能测试。
  编译成功不能替代生命周期、二进制兼容或图形验收。

## 布局、对话框与终端

- Tree 独占布局失效和最近的 bounds/scale。新增该标记用于避免每次指针事件重排；漏置会产生旧几何，故控件变更入口统一使其失效。
  布局独立于像素提交；等待 frame 或 release 期间也可处理最新 configure 和命中。
- Display 的 mapped 只表示本连接已经成功提交首个 buffer；pending_parents 只保存尚不能发送的标准父关系。
  首次提交后发送并删除等待项，父/子销毁时撤销；缺少该阶段，未映射父窗口会被服务端按 xdg-shell 规范当作 null。
- xdg-dialog 对象独占 compositor 的模态提示，toplevel 销毁后附属对象 inert；对象编号复用不得恢复旧关联。
  客户端 Context 拥有 UI 父子生命周期与输入筛选，不把可选的 compositor 策略当成应用安全保证。
- 等宽 atlas 只在需要该文字角色时加载一份，Rc 保活绘制借用；缺少缓存会在终端输出时反复读入约 9 MiB 字体。
  蒙版缩放只遍历裁剪范围且不分配；本轮不执行性能测试，实际缩放成本尚待测量。
- terminal 的 VT 模型、输入队列和控件只在 UI 线程使用。队列上限 1 MiB；读端每轮最多消费 64 KiB，防止持续输出饿死输入。
  PTY 使用现有 SessionChild launcher，正常关闭在后台终止完整进程组并回收，异常退出仍执行兜底清理。

- GPU Content 的 OnceCell 只在 compositor 线程发布一份纹理，输入像素发布后不可变；每帧资源由 Scanout 引用到 flip 完成。
  无这份生命周期约束，客户端替换内容或退出可能提前关闭 host 正在取样的 GEM。没有按窗口 ID 维护的第二份纹理表。
  顶点缓冲按两个输出槽复用，覆盖前必须无 flip 在途；标准 EXECBUFFER/transfer 与 KMS 的 fence 顺序由 DRM owner 保证。
  GPU 热路径的测量项是首次上传、移动时合成/提交与 flip 完成；本轮依用户要求不执行性能或 UI 测试。

- 程序图标按 context 首次使用时读取，窗口装饰持有 Rc，不重复解码同一条目的像素；缺失时使用既有通用图标。
- foreign-toplevel 的 title/app_id/state 只由 Display 的 done/closed 更新。SDK token 与协议对象代际一起校验，
  不复制协议状态或把可复用 wire id 当作长期应用标识。列表选择由运行库保存，应用按需读取稳定条目标识。
- 媒体播放器只在 Running/Draining/Idle 之间转换；后台 PCM 完成前不启动下一次播放，连续 seek 只保留最新目标。
  窗口关闭先请求停止再异步等待完成；取消 Future 的 Drop 只发送停止请求，不在 UI 线程 join。
- Canvas 的 TAB_INPUT 明确请求原始 Tab（终端补全）；未设置时由框架执行焦点遍历，不在应用另维护一份修饰键状态。
- 公共 setter 在相同标题、文本、菜单或显示列表时不提交新绘制；菜单数据不变时保留正在进行的键盘/指针交互。

## 新控件与异步对话框

- Outline 只持有父子关系、展开状态和可见索引，文本仍在 Node.rows；父先子后、ID 唯一且深度不超过 64。
  数据刷新按稳定 ID 保留展开，折叠选择对应分支；与列表复用滚动与双击时序，不在应用持有展开镜像。
- TextEditor 唯一持有 Document、选择/光标、两个滚动轴和行宽索引。修改只替换受影响宽度，scale 变化重建。
  缺少宽度索引会在每个按键扫描全文；索引与 Document 在同一同步修改入口更新，无跨 owner 人工同步。
  ABI 文本读取只在调用时拼接自有副本，Rust Document 不跨边界；编辑动作和剪贴板复用单行控件通知路径。
- Dialogs 与 Application 共用 Rc<RefCell> 窗口表和订阅 owner，不复制登记表；只允许 UI 线程使用。
  Future 可在 await 之后开下一项对话框；所有借用在返回 Future 之前结束，不跨 await 保留可变借用。
- 文件对话框 worker 不持有 Tree/Wayland/UI 句柄。请求和结果 channel 各限 32，结果仅凭单调窗口编号/请求序号接受。
  关闭不 join，旧结果丢弃，Context 释放 channel 后 worker 退出；共享通用控件处理输入与布局。
- WindowHost 只因装饰、configure 和 focus 的视觉变化失效；Tree input 单独返回 repaint，普通未捕获 motion 不重绘。
  多行选择拖动、菜单、范围和表头抓取仍申请帧；WindowHost 保留尚未成功提交的失效。
- calendar 的 OnceLock<RwLock<TimeZone>> 唯一持有已解析 TZif，文件 I/O 和解析均在锁外。
  reload 失败保留旧模型，固定偏移写入使用同目录 create_new 临时文件、sync 与 rename；没有进程环境写入。

- 列表的 mode、实际多选集合、范围锚点、原地重命名阶段和滚动只由 Table/Node 持有，应用查询稳定行 ID。
  ItemGrid 只投影同一几何，不扫描不可见行绘制；四种视图的命中、导航和滚动消费它的结果。
- PopupMenu 的节点持有同一个 MenuBar 状态机；Future 完成或取消即撤销节点，不嵌套运行循环。
  MenuVisual 仅复制影响像素的无分配投影，便于共享运行库判断是否重绘，不增加第二份可写状态。
- Calendar 持有选定日期和当前查看月份，两者意义不同；所有日期必须属于公历 1..9999 年的合法月份。
  分组框、标签头和内容区由同一布局定义；隐藏页不参与表头命中，删除选定页后清除旧选择。
- TabStrip 是标签页几何的唯一 owner，随布局写入 Node；绘制、命中和页面排版只消费它，不另算偏移。
  选中表头覆盖相邻表头与主体上边框，命中先测选中项。Painter::tab 只画调用者给出的矩形。
- Outline 的 RowGeometry 是树项缩进、展开框、图标与标签位置的唯一来源。Branch.first/last 在发布树数据时一次标记，
  缺少它们绘制分支线须逐行扫描兄弟表（最多百万行）或把末项竖线画到空处；树数据替换时随 Outline 整体重建。
- Sheet 的 Applying 阶段持有暂缓关闭位；关闭请求不丢弃，applied 成功后才发 CLOSE，失败保留窗口。
- GPU 方向约定由 linux-uapi 的 VirGL owner 固定：Y_0_TOP 只用于扫描目标，viewport 不取负 Y 比例。
- Window/Widget/dialog 的 ABI 编解码分别位于 window_api、widget_api、dialog_api；context_api 拥有等待和 FD 入口。
  这些模块只解释进程内公开 ABI，不引入 libc/设备 FFI；唯一可见动态符号仍是 liteos_ui_get_api。
- desktop-settings 通过标准 flock 串行化跨进程读改写，UI 必须在后台调用；各属性页仅修改自己拥有的字段。
  发布采用同目录 create_new 临时文件、sync 和 rename；正常错误清理临时文件，锁随 OFD 析构释放。

- 文件对话框的 Filter 只拥有复制后的标签与扩展名；Tree 的“所有文件”勾选是唯一筛选模式状态。
  worker 先筛选普通文件再计入 4096 项额度，目录始终可导航；请求序号排除切换前的结果。
  原有 ABI 槽位保留，新增筛选入口只追加到表尾；手工路径不受列表筛选限制。
- Wayland wire 以头部 16-bit 长度和四字节对齐接收至 65532 字节的消息；socket 分批读取不限制消息长度。
  编码/解码拒绝内部 NUL；编码错误原子回滚消息及其 FD。动态窗口文本在分配角色前验证。

- Scanout::Backend 只持有一种输出后端；back/pending/sequence 由 Scanout 唯一拥有。
  BootSurface 仅为启动位图投影 CPU/GPU 目标的可写像素行，不拥有协议或第二份像素存储。
  GPU boot transfer、合成与 page flip 均使用同一对资源；目标 Drop 先移除 FBO，再释放 GEM。
  Benchmark 决策：本轮按用户要求不运行性能验证；代码删除两块冗余全屏缓冲，不据此宣称帧率提升数值。
