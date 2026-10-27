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
- 新热路径的测量项为缓冲复用、任务调度/唤醒、布局与可见行绘制；验证入口与范围见[Wayland 验证](../development/wayland-ui-verification.md)。
  编译成功不能替代生命周期、二进制兼容或图形验收。

## 布局、对话框与终端

- Tree 独占布局失效和最近的 bounds/scale。新增该标记用于避免每次指针事件重排；漏置会产生旧几何，故控件变更入口统一使其失效。
  布局独立于像素提交；等待 frame 或 release 期间也可处理最新 configure 和命中。
- Display 的 mapped 只表示本连接已经成功提交首个 buffer；pending_parents 只保存尚不能发送的标准父关系。
  首次提交后发送并删除等待项，父/子销毁时撤销；缺少该阶段，未映射父窗口会被服务端按 xdg-shell 规范当作 null。
- xdg-dialog 对象独占 compositor 的模态提示，toplevel 销毁后附属对象 inert；对象编号复用不得恢复旧关联。
  客户端 Context 拥有 UI 父子生命周期与输入筛选，不把可选的 compositor 策略当成应用安全保证。
- 等宽 atlas 只在需要该文字角色时加载一份，Rc 保活绘制借用；缺少缓存会在终端输出时反复读入约 9 MiB 字体。
  蒙版缩放只遍历裁剪范围且不分配；独立缩放成本尚未测量；不由整体 canvas 基线推断。
- terminal 的 VT 模型、输入队列和控件只在 UI 线程使用。队列上限 1 MiB；读端每轮最多消费 64 KiB，防止持续输出饿死输入。
  PTY 使用现有 SessionChild launcher，正常关闭在后台终止完整进程组并回收，异常退出仍执行兜底清理。

- GPU Content 的 OnceCell 只在 compositor 线程发布一份纹理，输入像素发布后不可变；GPU.frame_contents 独立持有本帧全部 Content 到 flip 完成后的下一次 render，不能用单个采样视图代替多个 wl_buffer 使用期。
  无这份生命周期约束，客户端替换内容或退出可能提前关闭 host 正在取样的 GEM。没有按窗口 ID 维护的第二份纹理表。
  顶点缓冲按两个输出槽复用，覆盖前必须无 flip 在途；标准 EXECBUFFER/transfer 与 KMS 的 fence 顺序由 DRM owner 保证。
  GPU 热路径的测量项是首次上传、移动时合成/提交与 flip 完成；显示节拍与第三方 EGL 路径由 GPU gate 验证，交互由 Wayland/桌面 gate 验证。

- 程序图标按 context 首次使用时读取，窗口装饰持有 Rc，不重复解码同一条目的像素；缺失时使用既有通用图标。
- foreign-toplevel 的 title/app_id/state 只由 Display 的 done/closed 更新。SDK token 与协议对象代际一起校验，
  不复制协议状态或把可复用 wire id 当作长期应用标识。列表选择由运行库保存，应用按需读取稳定条目标识。
  同一桌面的普通应用可使用标准 foreign-toplevel，任务管理器依赖该能力；layer-shell 只授予 Session/Shell，
  security-context manager 只授予 Session。公开 socket 的 root 身份也不能获得后两者。
- 媒体播放器只在 Running/Draining/Idle 之间转换；后台 PCM 完成前不启动下一次播放，连续 seek 只保留最新目标。
  窗口关闭先请求停止再异步等待完成；取消 Future 的 Drop 只发送停止请求，不在 UI 线程 join。
- Canvas 的 TAB_INPUT 明确请求原始 Tab（终端补全）；未设置时由框架执行焦点遍历，不在应用另维护一份修饰键状态。
- 公共 setter 在相同标题、文本、菜单或显示列表时不提交新绘制；菜单数据不变时保留正在进行的键盘/指针交互。
- Canvas 入口按已定义的 kind 1～7 校验并原子发布整批命令；kind 7 的墙纸预览与 SDK、renderer 使用同一契约。
  背景页仅在墙纸/样式/颜色选择或预览逻辑尺寸变化时重建绘图列表；最后成功发布的尺寸由 UI 线程共享，
  不将仅改变激活状态的 Configure 当成草稿修改或失败重试。浏览任务与页面事件共用该记录，失败不推进尺寸。
  成本决策：只增加每页一次的尺寸记录分配，无逐帧工作或新 I/O；由实际壁纸选择/显示方式/取消路径的桌面回归验证。

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
- Window/Widget/dialog 的 ABI 编解码分别位于 window_api、widget_api、dialog_api；image_api 拥有 Image 的资源选择和调色板入口，context_api 拥有等待和 FD 入口。
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
  Benchmark 决策：沿用 canvas 与 GPU gate；删除两块冗余全屏缓冲，不据此宣称帧率提升数值。

## comctl32 控件、级联菜单与状态栏

- MenuBar 唯一持有弹出层栈、等待展开的子菜单（含预先算好的几何）与客户区边界；子菜单路径由条目索引表示，
  菜单数据替换即整体关闭。缺少 pending 只能悬停即开或丢失延时；缺少 bounds 子菜单会越出窗口。嵌套上限
  `MENU_DEPTH` 由 ABI 声明，运行库静态断言与共享状态机一致，并在复制菜单时校验父条目先于子条目。
- MenuItem 追加 `parent` 字段、state bit3 为默认项；产品处于开发期，按用户决定不保留旧布局。新增 ABI 槽位
  status_parts、background 追加到表尾。
- Toolbar 持有按钮矩形、悬停/按下与工具提示计时；同一组按钮刷新状态时保留交互，按钮组变化才复位。
  Tree.animated 记录上次推进定时状态的时刻，使提示只在越过显示时刻时重绘一次；缺少它会每次唤醒都重绘。
- 尺寸手柄：Tree.size_grip 由窗口 owner 每次布局写入（可缩放且未最大化/全屏），状态栏只画；Decoration.grip
  是同一矩形的命中副本，由 layout_record 在同一入口刷新，按下发起 xdg_toplevel.resize(BottomRight)。
- Table.cells 是图标/列表视图条目格子的唯一来源（布局时由主题图标间距与最宽标签写入），绘制、命中与重命名框共用；
  Table.full_row 只由 list_style 标志位设置。list_style 的第二个参数由布尔多选改为标志位（多选、整行选择）。
- Node.updown 只持有按住方向与下一次重复时刻，数值仍由 range/position 唯一持有；缺少重复时刻按住不会连续步进。
  定时推进返回的 CHANGED 由 Context 收集后入应用事件队列。CalWnd 的 Calendar 只持有选定日期，显示月份即该日期所在月。
- Node.status_parts 与 Node.background 只保存复制后的分格宽度、底色和共享位图引用；位图由资源上下文保活。
- 应用自建对话框经 Dialogs::window 登记到同一窗口表与订阅 owner，窗口与事件流的 Drop 撤销登记。
- Benchmark 决策：新增定时状态只在存在截止时刻时参与等待，菜单/提示的命中与绘制为常数级；沿用现有 canvas/GPU 基线，不新增该操作的 wall-clock 阈值。

## 下标位图与调色板

- Bitmap 唯一解码入口按签名识别 PNG、Windows DIB 与 JPEG；DIB 覆盖 CORE/INFO/V2–V5 头，1/4/8/16/24/32 位与
  BI_RGB、BI_RLE4/8、BI_BITFIELDS/ALPHABITFIELDS。只有位域声明 alpha 才用 alpha；RLE 跳过像素保持透明；
  未压缩调色板图保留下标供重新着色；越界调色板下标按 GDI 显示为黑色。JPEG 固定 zune-jpeg 0.5.15，输出 RGB。
  `load` 用于界面图像（单边 2048、输入 32 MiB）；`load_picture` 用于用户图片（单边 16384、2^24 像素、输入 64 MiB），
  上限在分配像素前检查。
- `classic-ui::wallpaper` 是 Windows 2000 墙纸合成（居中/平铺/拉伸，最近邻）的唯一实现；桌面外壳与显示属性预览共用。
  Canvas 绘制命令 kind 7 为墙纸预览：矩形代表整个屏幕，text 为图片路径，color 为显示方式；运行库按
  `wl_output` 当前模式换算的屏幕逻辑尺寸缩小居中/平铺原图。
- `classic-ui::PictureLoader` 唯一拥有用户图片的引用缓存、在途任务与后台线程；Context 的 Canvas 与桌面 Art 共用该实现。
  UI 更新当前引用集合并只借用就绪位图；文件读取与 crate 内部的 `Bitmap::load_picture` 解码全部在懒启动的单个 worker 执行，
  不向调用方暴露同步用户图片加载入口。
  任务与结果通道各容量一，同时只派发一项；其余当前引用留在 UI。完成经非阻塞 socket 纳入原有 Display poll，
  不增加定时轮询；无 Wayland 事件时也推进结果和重绘，帧未就绪时保留失效。
  引用撤销立即移除缓存，旧任务仍占用唯一在途槽直至回收；重新引用同路径从 Pending 开始，旧 Loading 结果不能发布到它。
  失败按当前引用记录一次，不逐帧重试。Drop 断开通道且不 join，已开始的读取/解码可继续到返回，不回调已关闭窗口。
  Context 仅使含 kind 7 的窗口像素失效；Art 在路径改变或完成时清除背景合成缓存，等待期间只画背景色。
  解码输出独占预乘 ARGB；8-bit 输入以 Arc 保活不可变下标，Bitmap 因此可自然 Send，禁止 unsafe Send。
  重着色结果共享下标，不修改资源缓存内的源图；其他 UI 位图引用仍使用 Rc，不把原子引用计数带入逐帧绘制。
- Image 创建和换图共用 image_api::resource。新增 image_palette 只追加 ABI 表尾；输入为 256 项 0xRRGGBB，
  同步读取并生成像素，验证失败不替换节点图像。未安装的可选素材保持空白，其他控件或非下标图像返回错误。
- Benchmark 决策：后台解码沿用原来的输入/尺寸上限；队列不积压大图，跨线程转移像素不复制。
  引用收集沿用原 Canvas 扫描，新增 socket 通知与 Arc 下标克隆只在加载/重着色时发生；常规绘制仍借用已有像素。
  调色板转换只在选择改变时执行 O(像素数) 分配。沿用现有 canvas/GPU 基线，不新增 wall-clock 阈值。

## 窗口动画与合成几何

- Session 的 Animation 只拥有目标窗口标识、起止矩形、开始时间与是否还原；实际像素与纹理仍由已提交 Content 拥有。
  还原期间窗口留在 minimized 输入状态，合成单独投影整窗；结束再 raise。切换目标先完成已有还原，同目标反向从当前矩形继续，
  重复还原不重置计时，窗口销毁取消动画。没有每帧复制像素、重新排版标题或建立动画专用纹理。
- Drawable 的 rect 是 CPU/GPU 共同使用的物理目标矩形；整窗和所有子 surface 从同一个根矩形映射，不能独立插值。
  GPU 借用 Content 的现有纹理，只改变顶点；CPU 的 blit_scaled 原地最近邻采样，整数等比尺寸保留既有快速复制路径。
  DRM flip 在途仍禁止覆盖 back，纹理由 GPU 采样视图保活到完成；不改变标准 Wayland commit/damage/frame/release 语义。
- 动画跟随 DRM flip 完成产生下一帧，不叠加固定 16 ms 轮询；动画放在普通窗口之上、系统外壳上层之下，不能遮盖安全覆盖层。
- Benchmark 决策：去除动画每帧标题光栅/像素 Vec 分配/纹理上传，GPU 增量为每个 surface 的矩形和顶点变换；
  CPU 的任意缩放只扫描可见目标像素，不分配中间图像。整窗交互由既有门禁验证，动画主观流畅度及独立成本仍由目标 workload 验收。

## 内容尺寸、菜单与通知区域

- Trackbar 独占轨道与滑块绘制几何；SDK Slider 的 Scroll 位置和 sndvol 的 ALSA 快照共用 TrackbarDrag，
  抓取只保存初始指针/数值，按住滑块边缘不跳位，释放/失焦清除。绘制终点与输入映射不得分开计算。
- LabelEdit 独占原地编辑草稿；编辑期间该行只画图标，由编辑框取代原标签，pending/取消后恢复普通绘制。
- 桌面每轮按顺序处理至多 64 项输入后提交最终状态；弹出层的指针移动只失效该层，不重画背景/任务栏。
  成本决策：几何和抓取无堆分配、没有新增定时器；移除无关全屏绘制，沿用 canvas 与桌面交互门禁，不宣称实测帧率。

- Label 的自然高度只取实际字体行盒，Button/Edit 等交互控件才加自己的留白。Tabs 的首选尺寸取全部可见页的最大内容尺寸，
  加 TabStrip 拥有的表头/边框；测量、排版、绘制和命中使用同一几何，不能用固定的占位高度代替。
- WindowHost 为固定对话框保持声明宽度，按该宽度测量内容高度并在必要时扩大；不改变用户可缩放窗口的尺寸。
  Display 独占选定的 surface 尺寸，使用标准 xdg_toplevel.set_min_size/set_max_size 随下一次 commit 发布；
  最大化、全屏与交互缩放期间遵守合成器配置，不自行扩大。重排只在既有 layout_dirty/尺寸或 scale 变化时计算，不增加每帧测量。
- Bitmap::load_icon 统一按请求尺寸、较大原图、较小原图选择 16/32/64 源素材；加载时最近邻缩放到请求尺寸。
  Resources 与 AppIcons 共用该入口，缓存仍按逻辑名和请求尺寸保存。损坏素材不静默替换，整张 UI 位图不参与图标匹配。
- MenuMetrics 拥有经典弹出菜单三像素外框留白与字体行高加四的项高；Popup 与外壳 ContextMenu 共用度量。
  Painter 填满项的高亮行，不重复扣除外框；分隔线、文字和快捷键按共同内容矩形排布。
- network-config::ethernet_links 唯一解释以太网接口的管理状态、载波与驱动名；连接列表、连接属性/状态页与桌面共用不可变快照。
  链路建立不等价于互联网连通。desktop::network 独占一个采样线程、容量为一的结果通道和停止通道，500 ms 采样，
  UI 只非阻塞取结果；Drop 断开停止通道并 join，不遗留后台线程。读取失败不继续宣称上次链路已连接。
- 桌面通知区、时钟、音量、网络、翻页按钮与任务按钮由同一 TaskbarLayout 给出绘制/命中几何；隐藏时钟或没有接口时收回对应槽位。
- Benchmark 决策：图标重采样只在资源加载时分配，内容测量只在布局失效时执行，网络文件 I/O 全部在后台；无新增逐帧全树扫描。
  功能由现有 host/Wayland/桌面门禁裁决；逐项样式仍由用户验收。

## 应用条目与安装集成

- `desktop-entry` 独占 Desktop Entry 解析、XDG 应用目录优先级和文件启动选择；desktop 与 file-explorer 共用。
  用户目录先于系统目录，递归条目以相对路径中的 `/` 替换为 `-` 得到 ID；Hidden 用户条目遮蔽系统同 ID 条目。
  Exec 按 string 与命令行两层转义解码，不经过 shell；Path 是应用工作目录。NoDisplay 不进入菜单，仍可用于 MIME 关联和窗口图标。
- `/bin/wine` 只装配当前账户的 `~/.wine`、FEX 和标准 XDG 环境，exec 固定上游运行时。
  Wine 安装器/注册表/IShellLink/menubuilder 拥有应用安装与快捷方式事实；外壳不复制一份安装数据库。
- `classic-ui::Bitmap::application_icon` 拥有应用图标加载：绝对 PNG/BMP、XDG hicolor 的 NxN/apps PNG、pixmaps PNG 与经典系统 BMP。
  PNG 固定 png 0.18.0，输入不超过 32 MiB，图像不超过 2048×2048，解码预算 32 MiB；加载后转预乘 ARGB 并复用 Bitmap 的缩放和绘制。
  当前只支持此 PNG 查找子集，不声明完整主题继承、SVG 或 XPM 支持。
- desktop 每秒重读已安装条目，桌面目录变化时重读快捷方式并重建当前图标集合；卸载后不保留旧动态图标像素。
  Windows `.lnk` 与同名导出 `.desktop` 并存时只展示标准入口，原 ShellLink 仍由 Windows 拥有。
  NoDisplay/Hidden 等语义只由 desktop-entry 解释；开始菜单不得静默截断为固定程序数。
- 成本决策：PNG 解码与目录扫描只在加载/清单刷新发生，不进入逐像素或逐帧合成；不新增失真的渲染 benchmark。
  以有界解码回归、标准条目样本和真实 Wine 安装/启动验证消费链。

## 产品资源

- `scripts/extract_windows2000_assets.py` 拥有 ISO 提取与产品转换；ISO、PE 和中间文件只在 `.local/`。
- `assets/classic/manifest.json` 拥有产品资源身份；`scripts/classic_assets.py` 验证并返回唯一安装清单，
  kernel、rootfs 与开发镜像同步均消费受 Git 管理的文件。缺失、损坏和未登记文件必须失败，不读取本地覆盖。
- 只发布实际消费者需要的原生尺寸和格式；新增资源必须在导入器登记 ISO 来源，不引入原创或替代资源回退。

## 字体目录与缓存

- typeface::FontCatalog 拥有扫描到的字体成员与 `fontmap.ini` 映射；Resources 每进程加载一次并共享，主题重载清空。
  轮廓字体经 linux-uapi `ReadOnlyMapping` 只读 `MAP_PRIVATE` 映射，同一 TTC 成员共享一份映射；页按需载入并跨进程共享页缓存，
  不再每进程整文件读入。字体文件只以 rename 替换，不得原地改写或截断（映射的 unsafe 前置条件）。
  Wine 安装器从同一映射生成标准 Fontconfig 别名与系统字体目录配置，不维护另一份字体回退清单。
- OutlineFace 独占每个成员的 fpgm 结果；Typeface 独占本字号各成员的 prep 结果（按成员地址索引，chain 中的 Rc 保活）
  与已光栅化字形（上限 4096，满即清空）。缺少这些缓存会在每个字形重复执行字体程序；None 记录失败，避免反复执行。
- classic_ui::Fonts 拥有六个系统字体用途、每个 scale 的原版 Marlett 标题按钮字体与最多 16 个应用派生逻辑字体（`set_font`）；Painter 只借用，不复制。
  sfnt 优先 Unicode cmap；仅缺少 Unicode 时采用 Windows Symbol format 4，保持原始 PUA 码位。
- 字节码解释器只在给定指令流上执行，越界、栈错误与步数上限使本次程序失败并退回未 hinting 轮廓，从不 panic。

## Wayland 提交、采样与输入

- 协议对象 Data 仅在 surface 创建时分配其独占 Box，避免每个 keyboard/region 等对象携带 768 字节的 surface payload。
  commit/frame 路径不为该变更新增分配；对象销毁沿用现有 owner 与 buffer release 路径，协议测试覆盖生命周期。

- Surface 独占 pending/current/cached。State 冻结 transform、scale、viewport、输入区域、内容偏移与子树顺序；
  Committed 同时保存 frame 回调和父提交冻结的孩子位置。surface_commit 是唯一提交/发布 owner，xdg/layer 只处理角色语义。
  wl_subsurface 的 sync/desync 即时生效并继承祖先模式；同步缓存以不可变 Rc 像素合并，不复制正在显示的整张图。
  Committed.unmapping 保留最后一次显式 null attach 的请求含义；没有内容不等于撤销映射，普通初始无 attach 提交必须发送角色 configure。
  layer_check_commit 只验证 pending，layer_after_commit 才发布角色 current 并与旧状态比较；否则首次协商或尺寸重协商会丢失。
- Attachment 持有 attach 时真实 Buffer（SHM 或 dma-buf）的 Rc；wl_buffer 对象销毁不使引用悬空，release 只发送给仍关联同一 Rc 的活对象。
  完整 SHM 快照完成后即允许 release；失败返回截断文件或 no_memory，不以空图像替代。
- Pixels 拥有 SHM 快照或 dma-buf BufferUse 与 OnceCell GPU 纹理，Content 只拥有共享像素引用与投影元数据；viewport 不再保存另一份 raw 内容。
  null attach 使内容为空，取消 viewport 不能恢复旧缓冲。CPU/GPU 共用 ImageSource 三点仿射坐标与逻辑尺寸，
  按 transform、scale、viewport 顺序解释参数；裸 buffer 尺寸错误归属 wl_surface，viewport 源越界错误归属 wp_viewport。
- Subsurface.children 只索引存活关联，State.order 唯一拥有提交后的绘制顺序。创建、位置、堆叠在父提交后可见；
  销毁关联立即取消映射，保留永久角色，父销毁断开引用，编号重用不能复活关系。
  深度预算包含祖先和已有后代；超预算以标准 no_memory 失败，不能在绘制时悄悄漏掉深层节点。
- Region 以 COW 不可变快照实现复制语义；每对象最多 4096 条加减操作，超限标准 no_memory，防止绕过对象数量限制耗尽内存。
  hit_testing 按提交树逆序和输入 region 命中，空洞穿透；实际 leaf 收指针，根窗口收键盘/激活，隐式抓取保留 leaf 坐标原点。
- queued_callbacks 等待显示提交，in_flight_callbacks 只属于已送交 Scanout 的帧；finish_frame 不能消费 flip 在途期间新提交的回调。
  presentation 显式登记可见树和实际呈现的客户端硬件光标，隐藏 surface 不提前获得下一帧许可。
- GPU.views 以真实资源编号与采样格式为键；VirGL 对象句柄从固定状态句柄之后独立分配，销毁命令先于句柄复用。
  上一帧 flip 完成后才能淘汰视图/覆盖顶点，exec 成功后才发布新缓存。没有逐四边形重复资源表或每帧重复创建采样视图。
- Session 的 applied_client_cursor 只保存成功上传的不可变图像引用及物理热点；同图像/热点跳过上传，形状切换或隐藏清空，
  位置仍独立更新。set_cursor 校验本连接最近 enter 序号后才分配角色，后续 offset 从设置时热点扣除累计增量。
- Benchmark 决策：移除 viewport 全图重采样、窗口元数据查询的子树分配、逐帧采样视图创建与未变化光标上传；
  CPU 仿射路径只扫描可见目标像素，不分配中间图像。相同 set_text 不使布局失效。
  验证范围见[Wayland 验证](../development/wayland-ui-verification.md)，不声明已提高多少帧率；原生控件仍走 wl_shm，damage/opaque 当前只是可忽略的提示。

## 键盘对象与迟绑定

- Seat 唯一拥有 logical keyboard focus、pressed keys 与 modifiers；Client 的每个 wl_keyboard 只保存是否已投递 enter 的 wire 状态。
  新资源先取得 keymap/repeat_info；即使 logical focus 未变化，也必须由 Seat 用当前真实状态补发 enter/modifiers。
  已 enter 的旧资源不能重复 enter，尚未 enter 的资源不接收按键；release 删除资源，surface 先销毁时只清理 wire 状态、不发送失效引用。
- `Server::keyboard_needs_enter` 只读检查资源状态，不 flush socket、不维护另一份焦点或按键表；正常焦点改变仍先 leave 再 enter。
  成本决策：无新锁、缓存或逐帧分配，查询为现有连接/对象表的有界只读遍历；沿用控件/Wayland 门禁，以 wire 顺序、迟绑定和 Wine 输入验证，不另设 wall-clock 阈值。

## linux-dmabuf / EGL 客户端

- Server 只在真实 VirGL context 可用时公布 zwp_linux_dmabuf_v1 v4；协议由固定 XML 生成，禁止私有消息。
  params 唯一拥有收到的 FD，校验平面数、使用一次、尺寸、格式、modifier、offset/stride，再调用 linux-uapi PRIME importer。
  async create 导入失败发送 failed；create_immed 导入失败发送 invalid_wl_buffer。错误路径释放全部 FD 与未发布资源。
- feedback 表使用内容与尺寸均封印的 memfd（F_SEAL_WRITE/GROW/SHRINK/SEAL），发送前填充一次，排队 FD 后不保留写入 owner；main_device 是实际 DRM dev_t，单 tranche 只声明 ARGB/XRGB 隐式 modifier，无 scanout flag。
- BufferUse 跨越 current/cached/in-flight Content，DmaBuffer 只持 Weak；最后使用退出排队 release，发送前核对对象 identity 和当前使用期。
  同一缓冲多次提交、wl_buffer 销毁或编号重用都不能提前 release。GPU.frame_contents 保活协议使用期，sampler view 只拥有 GPU 资源。
- linux-uapi 的 GemOwner 唯一 GEM_CLOSE；每 context 弱 handle 表合并重复/self-import，允许同一 handle 的 ARGB/XRGB 视图共存。
  导入不执行 CPU copy/upload；Y_INVERT 参与通用仿射源坐标。CPU readback 只在确有 CPU 消费时按提交缓存，不复制普通 GPU 合成。
- Mesa/GBM/EGL 属于上游客户端运行时，不替换 Rust 控件 ABI，不表示现有 classic-ui 控件已经迁移 GPU 光栅。
