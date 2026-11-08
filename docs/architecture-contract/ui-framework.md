# UI 框架与 SDK 契约

当前实现见[UI 架构](../architecture/ui-framework.md)；Wayland 固定版本由[规范基线](../standards-baseline.md)维护。

## Owner 与依赖

- linux-uapi 只拥有 Linux libc/UAPI mechanism；classic-ui 拥有标准 Wayland 客户端、绘制与 WindowHost。
- classic-ui-abi 是无运行期状态的 no_std 布局声明；classic-ui-runtime 依赖该声明与 classic-ui，实现共享控件。
- liteos-ui 只依赖 ABI 声明，提供安全 Rust 生命周期、事件、Future 与回调适配，不静态复制控件实现。
- Display 唯一拥有协议对象、configure、callback、缓冲与释放；WindowHost 不复制这些状态。
  `paint_frame` 只借出已释放缓冲的像素。回调获知内容是否为上一帧，内容不保留时完整重绘；
  局部绘制返回物理像素损伤，初始映射强制声明全幅损伤。失败使该缓冲的比较基线失效。
  首次直接绘制前预分配完整 memfd 存储，使限额和内存不足在绘制前返回；借用期间缓冲移出 Display 表。
- Context 唯一拥有窗口表、控件树、资源与 FD interest；对象标识单调递增不复用，迟到事件不能命中新对象。
- Display 的 ImageId 拥有原尺寸 SHM pool/buffer 与只读映射；发布前核验 F_SEAL_WRITE/GROW/SHRINK，失败不建立映射或发表 FD。
  多表面共用不可变 wl_buffer 时不依赖 release 次数或顺序，不重写/复用其存储；destroy 后服务端 Content 与在途图形引用继续保活。
  合成器 BufferSpec 的懒快照仅用于已核验封印的 pool，单连接线程、固定后端初始化一次；可变 pool 保持原 damage/COW 路径。
  没有该缓存，平铺会逐子表面复制同一像素；缓存不复制 viewport/source 等每表面几何，也不扩大为全局图像表。

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
6. 同主版本兼容更新保留旧符号、布局与语义；应用重启后消费新实现，不热替换存活对象。
   安装以原子替换发布共享库，禁止原地覆盖已映射文件。字体、主题、图标等资源保持旧 schema 可读，
   或保留旧进程所需版本；仅替换 .so 而使旧进程资源失效不满足兼容契约。

SDK 只允许 connection.rs 导入 liteos_ui_get_api；ABI 声明与运行库导出没有 libc/设备 foreign import。
原始 OS/设备 FFI 仍只允许位于 linux-uapi，不能借公开 SDK 边界扩大到其他目录。

## 事件、等待与退出

- 控件已消费的按钮、滚轮与指针事件不再按底层 hit 生成应用原始事件；键盘事件保留 handled，供应用筛选快捷键。
- 任务管理器后台连续采样状态栏与性能历史。只有 GPU 页请求 DRM 描述符扫描，UI 线程用最近快照更新当前页。
  切页立即投影已有数据；GPU 页首次打开请求一次采样，暂停后保留已有 GPU 快照，F5 显式刷新。
  成本以实际来宾同一页的读取次数与 CPU 时间比较，不设宿主时序阈值。
- wl_surface.frame 与 wl_buffer.release 独立；buffer 忙时不覆盖，在途失效只在提交成功后消费。
- 交互缩放期间，Display 可在已 release 的 `wl_shm_pool` 上重建几何不同的 `wl_buffer`，新规格整幅写入。
  每窗口仍最多三个缓冲槽，每槽容量至多比当前绑定几何多 256 KiB。超出范围即替换存储，缩放结束退休不同尺寸的空闲槽，
  在途槽等待 release 后退休。以同一来宾、窗口和拖动轨迹比较共享内存创建次数，帧延迟另行测量。
- `Display` 的每窗口 `History` 持有最近提交的缓冲编号、最近 3 次损伤与对应规格（像素尺寸、scale）。
  `present` 直接借用该缓冲的共享映射逐行比较新画面（相隔 ≤8 行的变化合并成条带，超过 16 条合并为外接矩形），以 `damage_buffer` 声明。
  memfd 尺寸已封印，合成器只读取，客户端只在 release 后写入，比较完成即结束借用。
  每块缓冲记录自己持有第几次提交；复用时补写其后各次与本次的损伤，内容未确认、规格变化或落后超出历史则整幅写。
  开始写入时先取消该缓冲的提交记录，写入或请求编码中途失败后，部分更新的内容不会被当作比较基线。
  画面不变且已按同一规格映射时只发 frame + commit、不 attach，configure 的 ack 仍随 commit 生效。
  损伤行直接交给 memfd 批量校验与写入，沿用其文件描述符。每槽增加共享映射及相应的页表成本，释放原先每窗一份独立历史像素及其复制；
  内存和交互以同一输出尺寸的来宾采样裁决。
  Layer unmap 清空比较基线但保留递增序号，避免晚到 release 的缓冲跨映射代际被误判为最新画面。所有调用 `present` 的程序共用这一路径，
  外部像素的 present 仍由调用者提供完整画面；WindowHost/Tree 的区域光栅使用下文 paint_frame 契约，不混同这两类内容 owner。
- 普通桌面状态的登录兜底用一个不透明像素和 `wp_viewport.set_destination` 保持整屏映射与安全注意序列路由。
  安全页的整屏缓冲与撤销 viewport 在同一 `wl_surface.commit` 生效。尺寸切换后空闲旧缓冲立即退休，忙缓冲等待 release，
  切换到单像素帧时比较基线也切换到单像素缓冲。成本用同一分辨率下登录进程驻留、图形资源及锁屏往返测量。
- 桌面外壳指针移动由实际交互标记受影响的图层。空白背景移动与任务栏提示计时不触发光栅，图标拖动、编辑选区及菜单变化只标记对应图层。
  成本以真实来宾同分辨率的指针移动和交互帧诊断，宿主微基准不代表重绘或显示延迟。
- 桌面 Scene 拥有同步墙纸/图标/框选子表面；root 仅持单像素底色，子表面空输入 region 保留根的命中、键盘与拖放职责。
  小平铺图案按整数倍预重复到接近 256×256，额外图像最多 65536 像素，避免成千上万协议对象；大图保持原尺寸。
  图标层按实际图像/标签/编辑框包围盒分配；框选只分配四条一逻辑像素的边，按根坐标恢复墙纸/图标并保持 XOR 刷相位。
  旧覆盖与新覆盖参与局部擦除；几何或 backing 变化完整初始化局部层。替换先提交子表面 null attach，父 commit 后销毁旧角色，避免中间空白。
- 文件拖放由桌面按共用图像和标题几何接受回收站目标。Wayland 收到 drop 后以当时接受的 surface 交付完整 URI 列表，
  不用完成读取时的桌面布局重新命中。外部拖放只有目标高亮变化才重绘桌面；格子外直接排除，标签仍复用现有度量。
  URI 列表与文件剪贴板共用 4096 个路径及 128 KiB URI 数据的上限。桌面通过只读 memfd 作为标准输入交给独立删除确认，
  避免 exec 参数指针和环境占用缩小拖放容量。拒绝无效或超限的完整批次；资源管理器按原顺序复核、确认和回收。
- 窗口还原/激活立即更新窗口栈与键盘目标，整窗几何动画只影响合成；不能等动画结束再撤销最小化，否则桌面兜底焦点会吞掉后继按键。
- Desktop 唯一拥有图标焦点、排序且去重的选择索引及 Shift 锚点；焦点与选中状态独立，Shift 图标选择按网格矩形而非目录行号。
  目录刷新先结束抓取，再按 Target 身份重建焦点、选择和锚点；粘贴完成引起的刷新也遵守这一顺序。
  Drag 只能是整组移动或框选之一，框选保留原集合供 Ctrl 切换和 Esc 恢复，抓取期间不执行文件快捷命令。
  桌面普通删除、永久删除和回收共用 URI/memfd 输入及独立确认，不因多选另建文件事务。
  成本决策：选择容量受桌面条目上限约束，移动事件复用集合；占用索引仅在释放整组移动时临时构造。
  虚线框只恢复、提交四边，沿用已有背景及图像/标签损伤，不增加整屏像素副本或计时器。以真实批量剪贴板、框选和拖入回收站验收，不新增微基准。
  最后一个普通窗口退出且没有显式图层焦点时，Session 从既有 Background/OnDemand 图层恢复桌面键盘目标；
  与桌面快捷键复用同一选择规则，排除登录客户端，独占与安全握手仍优先，不另存焦点历史。
- 外部 PollFd 带 fd owner 的类型借用；批量等待后只投影 readiness，不延长其他描述符的生命周期。
  PollBuffer 只持有不可自行轮询的 raw 存储；每轮 PollSet 重新建立有效借用，析构清空旧编号并保留容量。
  唯一切片转换位于 linux-uapi，依据 repr(transparent) 与 push 借用前置条件；不建立 static FD 或从旧轮次复活编号。
- 资源管理器在同一个 inotify FD 上先注册目标目录再读取列表，切换期间保留原目录的 watch，读取完成后按结果保留其一。
  已退休 watch 的排队事件不刷新当前列表，队列溢出仍触发重新读取。
- Transport 独占输出片段及其 FD，非阻塞发送保留部分写偏移，首次正数 sendmsg 后移除 FD，后续仅发送余下字节。
  普通 Writer 尾部在带 FD 请求前入队，保持 wire/SCM_RIGHTS 顺序；上限错误是连接失败，不能重试重放 commit。
- Waker 的 AtomicBool 合并重复登记，Mutex 只保护短队列；不在锁内 poll Future 或调用应用代码。
  入队后写非阻塞 socket，满 socket 已有未消费通知；注销后的唤醒无 UI 对象访问。
- Background 的阻塞队列上限 64、并发上限 2；工作者按需启动，空闲 5 s 退出至零，不留周期扫描线程。
  入队、工作者计数与退休同锁，任务和 UI Waker 始终锁外执行；首个线程创建失败不接收任务，扩容失败仍可由现有工作者推进。
  提交句柄独立于工作者的 Arc 寿命计数，最后一个句柄退出时唤醒全部空闲线程，已接收任务仍按原语义排空，不在 UI join。
  文件对话框的专属队列/通知 watch 在最后一个对话框关闭时退休；创建窗口失败也回收，旧结果不能投递给下一代 watch。
- Task/窗口关闭后，Future 在 UI 线程释放；FD interest、定时器与接收端通过各自 RAII owner 撤销。
  在途 clipboard 传输和协议缓冲不依赖已释放的回调 userdata。
- 协议断开向应用返回错误；库不调用 process::exit。Application 析构取消任务、关闭窗口，再释放连接。
- 新热路径的测量项为缓冲复用、任务调度/唤醒、布局与可见行绘制；验证入口与范围见[Wayland 验证](../development/wayland-ui-verification.md)。
  编译成功不能替代生命周期、二进制兼容或图形验收。

## 布局、对话框与终端

- Tree 独占布局失效和最近的 bounds/scale。该标记避免每次指针事件重排；影响几何的变更入口必须置位，否则保留旧几何。
  可变借用本身不作废布局。结构、文本/字体、尺寸规则、边框/隐藏/可用状态、列表内容/图标/复选框占位、
  列表视图模式、树展开与标签页选择等实际几何变化才置位；选区、颜色、Canvas 命令或视频帧、数值和动画只失效绘制。
  相同列表、列定义、字体等更新保持已有布局和交互状态；configure 的 bounds/scale 变化仍独立强制重排。
  区分几何更新与像素更新参照 Qt 6.8 的 updateGeometry/update 职责；不新增缓存、分配或绘制路径，
  因而不另设 benchmark。逐项审查修改入口及其尺寸依赖，运行期只针对标签页、视图切换和缩放核对几何。
  布局独立于像素提交；等待 frame 或 release 期间也可处理最新 configure 和命中。
- Display 的 mapped 只表示本连接已经成功提交首个 buffer；pending_parents 只保存尚不能发送的标准父关系。
  首次提交后发送并删除等待项，父/子销毁时撤销；缺少该阶段，未映射父窗口会被服务端按 xdg-shell 规范当作 null。
- 顶层窗口销毁后，已在途的 configure 仅被解码，活窗口才发送 ack；否则无效请求会断开同一应用的整条连接。
- xdg-dialog 对象独占 compositor 的模态提示，toplevel 销毁后附属对象 inert；对象编号复用不得恢复旧关联。
  客户端 Context 拥有 UI 父子生命周期与输入筛选，不把可选的 compositor 策略当成应用安全保证。
- 等宽 atlas 只在需要该文字角色时加载一份，Rc 保活绘制借用；缺少缓存会在终端输出时反复读入约 9 MiB 字体。
  蒙版缩放只遍历裁剪范围且不分配；独立缩放成本尚未测量；不由整体 canvas 基线推断。
- terminal 的 VT 模型、输入队列和控件只在 UI 线程使用。队列上限 1 MiB；读端每轮最多消费 64 KiB，防止持续输出饿死输入。
  PTY 使用现有 SessionChild launcher，正常关闭在后台终止完整进程组并回收，异常退出仍执行兜底清理。

- GPU Content 的 OnceCell 只在 compositor 线程发布一份纹理；SHM 像素写时复制，只有 surface 唯一持有时才被下一次提交原地更新。
  GPU.frame_contents 只持有本帧 dma-buf Content 到 flip 完成后的下一次 render，不能用单个采样视图代替多个 wl_buffer 使用期；
  SHM 已在 commit 读取后 release，若也被 pin，引用计数恒大于 1，每次提交都会整幅重读并重建纹理。
  无这份生命周期约束，客户端替换内容或退出可能提前关闭 host 正在取样的 GEM。没有按窗口 ID 维护的第二份纹理表。
  顶点缓冲按两个输出槽复用，覆盖前必须无 flip 在途；标准 EXECBUFFER/transfer 与 KMS 的 fence 顺序由 DRM owner 保证。
  GPU 热路径的测量项是首次上传、移动时合成/提交与 flip 完成；显示节拍与第三方 EGL 路径由 GPU gate 验证，交互由 Wayland/桌面 gate 验证。

- 程序图标按 context 首次使用时读取，窗口装饰持有 Rc，不重复解码同一条目的像素；缺失时使用既有通用图标。
- foreign-toplevel 的 title/app_id/state 只由 Display 的 done/closed 更新。SDK token 与协议对象代际一起校验，
  不复制协议状态或把可复用 wire id 当作长期应用标识。列表选择由运行库保存，应用按需读取稳定条目标识。
  同一桌面的普通应用可使用标准 foreign-toplevel，任务管理器依赖该能力；layer-shell 只授予 Session/Shell，
  security-context manager 只授予 Session。公开 socket 的 root 身份也不能获得后两者。
- 媒体播放状态由 libmpv 拥有，Engine 只镜像观察事件；播放/暂停命令在后端切换真实状态，不按迟到的 UI 快照重写。
  `mpv-client::render` 独占 render context，callback 只投递容量为 1 的唤醒；UI 不等待 render API，渲染线程不等待普通 client API 或 UI。
  关闭先撤销 client callback，再 stop/join 渲染 owner（撤销 render callback、释放 context），最后销毁 core 与动态库。
  尺寸请求与完成帧各只保留最新一项；短锁仅交换元数据/Vec，锁外渲染和复制。工作帧、已完成帧与 UI 借用帧至多三份存储，归还后复用；
  即使 UI 尚未取帧也继续服务 mpv 并替换完成槽，避免 core 等待渲染、UI 等待 core 的循环。关闭唤醒与更新共用队列，不能因队列满而丢失 stop。
  空闲绘图命令不能留在新帧之上。Benchmark 决策：保留软件渲染与原有呈现计时，增加有界存储换取 UI 不等待；交互延迟需用真实播放缩放测量，不以线程拆分推断性能改善。
  视频区单击通过应用定时器等待双击判定，双击仅切换全屏；显式命令取消未决单击，避免之后误暂停。
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
  可编辑组合框复用单行输入的编辑能力判断，程序化全选和文本剪贴板与键盘输入一致；下拉列表式组合框不接收文本编辑。
- Dialogs 与 Application 共用 Rc<RefCell> 窗口表和订阅 owner，不复制登记表；只允许 UI 线程使用。
  Future 可在 await 之后开下一项对话框；所有借用在返回 Future 之前结束，不跨 await 保留可变借用。
- 文件对话框 worker 不持有 Tree/Wayland/UI 句柄。请求和结果 channel 各限 32，结果仅凭单调窗口编号/请求序号接受。
  关闭不 join，旧结果丢弃，Context 释放 channel 后 worker 退出；共享通用控件处理输入与布局。
- WindowHost 只因装饰、configure 和 focus 的视觉变化失效；Tree input 单独返回 repaint，普通未捕获 motion 不重绘。
  多行选择拖动、菜单、范围和表头抓取仍申请帧；WindowHost 保留尚未成功提交的失效。
  WindowHost 不拥有画布：经 Display.paint_frame 在已释放共享缓冲中绘制，稳定态只光栅旧/新失效区；原 render_frame/整窗哈希模块已删除。
  Nodes 只提供只读 map 投影，get_mut/匹配可变访问推进 Node 的局部版本，不能取出原始可变 BTreeMap 绕过失效。
  每节点只保存上次成功提交的版本/覆盖/焦点与按压投影；删除时将旧覆盖移交 Tree.retired，等待/失败不确认基线。
  菜单弹层、组合框列表与提示的覆盖使用各自绘制 owner 的同一几何；菜单助记提示改变使全部相关客户区失效。
  异步图片完成显式使依赖的 Canvas 版本失效；普通动画计时只为真实视觉变化推进版本，不因扫描而重画全树。
  WindowHost 将损伤合并为最多 32 个不相交逻辑矩形；超出只扩大覆盖，每个矩形重建底色/装饰和相交控件子树，再发送物理 damage_buffer。
  布局重新计算时放置节点全部失效；颜色、选区、绘图命令和媒体帧等纯绘制变更只影响对应控件及旧/新覆盖层。
  新缓冲/不是最新完整画面、规格/装饰/主题变化时全幅初始化；失败由 Display 作废该缓冲内容，下一次不能沿用半幅结果。
  同次损伤、光栅与确认使用冻结的 now，防止提示在提交中途到期而被误标为已显示；成功提交后才确认 Tree 基线。
  callback ID 在绘制/分配/像素写入准备成功后登记，避免可重试绘制失败遗留永远没有 done/delete_id 的对象。
  稳定状态每窗口只保留一块空闲缓冲，第二块仅在上一块未 release 时临时分配、release 后退休；交互缩放保留空闲池复用几何。
  成本与验收范围由[性能覆盖](../development/performance-coverage.md)拥有；合并/背景重建参照 [Qt 6.8 QWidget paintEvent/update](https://doc.qt.io/qt-6.8/qwidget.html#paintEvent)，缓冲内容有效性沿固定 EGL_EXT_buffer_age 语义。
- calendar 的 OnceLock<RwLock<TimeZone>> 唯一持有已解析 TZif，文件 I/O 和解析均在锁外。
  reload 失败保留旧模型，固定偏移写入使用同目录 create_new 临时文件、sync 与 rename；没有进程环境写入。

- 列表的 mode、实际多选集合、范围锚点、原地重命名阶段和滚动只由 Table/Node 持有，应用查询稳定行 ID。
  ItemGrid 只投影同一几何，不扫描不可见行绘制；四种视图的命中、导航和滚动消费它的结果。
- 无表头列表可按稳定行 ID 投影复选框。控件共用绘制与命中几何，点击复选框时保留当前选中行，空格作用于选中行。
  `TOGGLE_ROW_CHECK` 携带目标行 ID。应用持有勾选状态，接受请求后重新发布。`row_checks` 追加到 ABI 表尾。
- `ColumnHeader` 独占实际列宽。`read_column_width` 按稳定列 ID 读取拖动或自动调宽后的值，资源管理器只在打开列设置时读取。
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
- 剪贴板 MIME 查询、读取和提供接口追加到 ABI 表尾，原文本接口保留。读取转移一个非阻塞 FD，由 SDK 的同一事件循环等待。
  运行库拥有当前选择与本地身份。条件替换要求身份匹配且仍有键盘焦点，供剪切粘贴清除已移动项目。
- 设置 `EDIT_LABELS` 的列表使用绘制时的标签几何判断慢速点击，经同一事件循环等待双击窗口结束后发出 `RENAME_REQUESTED`。
  应用用稳定行标识确认可编辑项，再走现有原位编辑与提交接口。控件不直接修改文件。
- 设置 `KEYBOARD_CONTEXT_MENU` 的列表用自身选择和视口解析 Shift+F10 与菜单键，发出 `CONTEXT_MENU`。事件携带稳定行标识和逻辑锚点，命令及可用性由应用决定。声明此能力的列表才消费这些按键。
- desktop-settings 通过标准 flock 串行化跨进程读改写，UI 必须在后台调用。属性页更新各自的字段，资源管理器只写当前文件夹的查看方式。
  发布采用同目录 create_new 私有临时文件、sync 和 rename；正常错误清理临时文件，锁随 OFD 析构释放。

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
- Bitmap 的系统 UI 像素来自构建期 LUIA0001 图集；`bitmap::atlas` 独占只读映射、验证及逻辑名称解析，
  `Pixels`/`Indices` 分别保活解码存储或同一映射。进程内 Weak 目录合并活引用，最后一个位图释放后 unmap；绘制不取目录锁。
  原始 BMP 输入及实际来源仍由 classic_assets 清单拥有，唯一转换器 `pack_shell_assets` 预乘、派生尺寸并打包；
  userland 唯一清单只安装图集，离线同步退休逐图 BMP，图集错误不回退逐图解码。
  文件只读且以 rename 更新，完整索引/范围/对齐先验证再构造 u32 视图；两个目标均为小端。
  重着色只分配新颜色像素，原始下标继续共享；替换媒体帧在分配成功后才放弃映射，不修改共享源。
  格式与迁移边界见[资源规格](../design/classic-desktop/assets.md#安装图集)，不新增应用接口或私有 OS ABI。
- `classic-ui::wallpaper` 是 Windows 2000 墙纸合成（居中/平铺/拉伸，最近邻）的唯一实现；桌面外壳与显示属性预览共用。
  Canvas 绘制命令 kind 7 为墙纸预览：矩形代表整个屏幕，text 为图片路径，color 为显示方式；运行库按
  `wl_output` 当前模式换算的屏幕逻辑尺寸缩小居中/平铺原图。
- `classic-ui::PictureLoader` 唯一拥有用户图片的引用缓存、在途任务与后台线程；Context 的 Canvas 与桌面 Art 共用该实现。
  UI 更新当前引用集合并只借用就绪位图；文件读取与 crate 内部的 `Bitmap::load_picture` 解码全部在按需启动的单次任务执行，
  不向调用方暴露同步用户图片加载入口。
  结果通道容量一，同时只派发一项，解码/通知后立即退出线程；其余当前引用留在 UI，不保留空闲解码线程。
  完成经非阻塞 socket 纳入原有 Display poll，
  不增加定时轮询；无 Wayland 事件时也推进结果和重绘，帧未就绪时保留失效。
  引用撤销立即移除缓存，旧任务仍占用唯一在途槽直至回收；重新引用同路径从 Pending 开始，旧 Loading 结果不能发布到它。
  失败按当前引用记录一次，不逐帧重试。socket EOF 只表示单次任务已退出，是否失去结果由结果通道判定。
  Drop 断开通道且不 join，已开始的读取/解码可继续到返回，不回调已关闭窗口。
  Context 仅使含 kind 7 的窗口像素失效；Art 在路径改变或完成时清除背景合成缓存，等待期间只画背景色。
  解码输出独占预乘 ARGB；8-bit 输入以 Arc 保活不可变下标，Bitmap 因此可自然 Send，禁止 unsafe Send。
  重着色结果共享下标，不修改资源缓存内的源图；其他 UI 位图引用仍使用 Rc，不把原子引用计数带入逐帧绘制。
- Image 创建和换图共用 image_api::resource。新增 image_palette 只追加 ABI 表尾；输入为 256 项 0xRRGGBB，
  同步读取并生成像素，验证失败不替换节点图像。未安装的可选素材保持空白，其他控件或非下标图像返回错误。
- Benchmark 决策：后台解码沿用原来的输入/尺寸上限；队列不积压大图，跨线程转移像素不复制。
  引用收集沿用原 Canvas 扫描，新增 socket 通知与 Arc 下标克隆只在加载/重着色时发生；常规绘制仍借用已有像素。
  调色板转换只在选择改变时执行 O(像素数) 分配。沿用现有 canvas/GPU 基线，不新增 wall-clock 阈值。
- Canvas 媒体帧与窗口全屏是追加到 ABI 表尾的两个入口。`canvas_frame(window, id, width, height, pixels, count)` 同步复制
  `width×height` 个设备像素（高字节被忽略并补成不透明）到 Canvas 独占的位图，等比居中绘在显示列表之下，空白补黑；
  同尺寸复用像素存储，帧缩到容量一半以下时尝试归还大块存储。清空释放帧，再次清空不触发重绘。
  非 Canvas、边长为 0 或超过 16384、`count` 不等于 `width*height` 返回 INVALID 且旧帧不变，`(0,0)` 清除。
  异步帧尺寸可落后于当前几何，Canvas 在同一绘制层采样旧帧，窗口配置与控件重绘无需等待新尺寸渲染。
  `fullscreen(window, on)` 只编码标准 `xdg_toplevel.set_fullscreen/unset_fullscreen`，结果由 Configure 事件通知。
  Benchmark 决策：每帧仍需一次 O(像素数) 复制与一次 blit；同尺寸提交在代码审查中确认无帧存储分配，不据此推断帧率。
- `combo_dropped_height(window, id, height)` 追加到 ABI 表尾，对应对话框模板 COMBOBOX 的 cy：展开总高度减去文字区为列表高度，
  按整行取可见项数，项目更少时收缩；0 恢复默认最多 16 行，负数或非组合框返回 INVALID。下拉列表画在本窗口表面内，
  打开时在窗口范围内定位，下方放不下且上方更宽裕时向上展开；仍放不下时减少可见行而不越出窗口。
  调用者：映射网络驱动器向导（netplwiz 模板 106）。Benchmark 决策：只在打开列表时做一次整数运算，不新增基线。

## 窗口动画与合成几何

- Session 的 Animation 只拥有目标窗口标识、起止矩形、开始时间与是否还原；实际像素与纹理仍由已提交 Content 拥有。
  最小化/还原与最大化/全屏/还原模式切换共用同一过渡：模式切换立即改窗口栈几何并发 configure，画面从旧矩形缩放到新矩形，
  客户端提交新尺寸后继续按插值矩形缩放；否则旧尺寸缓冲会先跳到新位置、等新缓冲才撑满。
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
  Enter、点击编辑框外或窗口失焦仅提交一次，应用确认前保留 pending；Esc 取消。
  应用拒绝提交时，在错误对话框关闭后恢复草稿，避免错误框引起的失焦再次提交同一操作。
  开始、接受或恢复草稿时，Context 撤销该节点未完成的文本粘贴及 FD 监听，迟到数据不能进入后续草稿。
  owner 与 ABI 不变；撤销仅在编辑寿命边界扫描已有的至多八个粘贴，不增加逐帧分配或定时器，无需新增 benchmark。
- Stack 在活动子窗口退出时沿已有 parent 链激活最近的存活、未最小化祖先；后台子窗口退出不改变活动窗口。
  父链取自退出前的窗口栈，支持父子同时退休，不添加焦点副本。成本仅在活动窗口退休时遍历既有栈，无新增分配与定时器；以真实错误框关闭后的输入目标验收。
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
- network-config::ethernet_links 唯一解释以太网接口的管理状态、载波、连接名与驱动名。连接列表、连接属性/状态页、桌面与 `ipconfig` 复用该快照。
  链路建立不等价于互联网连通。desktop::tray 统一拥有网络/USB 的单个后台采样线程、500 ms 截止与容量为一的组合快照，UI 只非阻塞应用结果。
  Network/Usb 只持 UI 视图，禁用接口仍有一个请求的有界准入；命令失败不阻止同轮 USB 更新，完成会失效通知区可用状态。
  Drop 断开唯一请求端并 join，不遗留后台线程。链路读取失败清除旧连接保证；USB 读取失败保留上次列表，下一次成功采样替换。
- 桌面通知区、时钟、音量、网络、翻页按钮与任务按钮由同一 TaskbarLayout 给出绘制/命中几何；隐藏时钟或没有接口时收回对应槽位。
- Benchmark 决策：图标重采样只在资源加载时分配，内容测量只在布局失效时执行，网络文件 I/O 全部在后台；无新增逐帧全树扫描。
  功能由现有 host/Wayland/桌面门禁裁决；逐项样式仍由用户验收。

## 应用条目与安装集成

- `desktop-entry` 独占 Desktop Entry 解析、XDG 应用目录优先级和文件启动选择；desktop 与 file-explorer 共用。
  类型来自 XDG `mime/packages/*.xml`，支持名称 glob 的权重/大小写、别名、继承、本地化名称和文件图标；
  当前不实现内容 magic 或编译 MIME cache。一次目录读取共用安装/关联快照，不逐文件扫描注册目录或读取远端内容。
  `mimeapps.list` 按标准 config/data 与 desktop 优先级处理 Default/Added/Removed；缺失或卸载的处理器被跳过。
  “打开方式”只列可接收文件的已安装条目，临时选择不改配置；设置默认在用户配置锁内合并后原子发布。
  原生执行需执行位与 ELF/shebang 文件头同时成立，不能把 SMB 投影的执行位当成文件格式。
  用户目录先于系统目录，递归条目以相对路径中的 `/` 替换为 `-` 得到 ID；Hidden 用户条目遮蔽系统同 ID 条目。
  Exec 按 string 与命令行两层转义解码，不经过 shell；Path 是应用工作目录。NoDisplay 不进入菜单，仍可用于 MIME 关联和窗口图标。
- 桌面的选择、刷新与重命名动作集中在 `desktop::icons`，交互状态仍由唯一 `Desktop` 持有，事件分派不复制状态。
- 桌面与资源管理器通过 `desktop-entry` 共用文件剪贴板编解码与文件事务。界面线程按选择身份核对数据，后台执行磁盘操作。已完成部分才从仍有效的剪切选择移除。
- `classic-ui::Bitmap::application_icon` 拥有应用图标加载：绝对 PNG/BMP、XDG hicolor 的 NxN/apps PNG、pixmaps PNG 与经典系统 BMP。
  文件类型图标沿用同一加载器的 `NxN/mimetypes` 上下文；原版资源由 ISO 导入器发布，生成素材由 `file_type_icons.py` 转换并登记摘要。
  行图标来源区分系统资源、应用 app_id 和主题名/绝对路径（RowIcon kind 0/1/2）；不得把 MIME 图标名送入 app_id 查找。
  主题图片只在一次批量发布内去重，不跨刷新持有缺失结果，新安装图标无需重启运行库。
  PNG 固定 png 0.18.0，输入不超过 32 MiB，图像不超过 2048×2048，解码预算 32 MiB；加载后转预乘 ARGB 并复用 Bitmap 的缩放和绘制。
  当前只支持此 PNG 查找子集，不声明完整主题继承、SVG 或 XPM 支持。
- desktop 每秒重读已安装条目，桌面目录变化时重读快捷方式并重建当前图标集合；卸载后不保留旧动态图标像素。
  注册应用变化时同时刷新普通文件的关联图标；类型数据与手工默认值更新可通过刷新重新读取。
  NoDisplay/Hidden 等语义只由 desktop-entry 解释；开始菜单不得静默截断为固定程序数。
- 成本决策：PNG 解码与目录扫描只在加载/清单刷新发生，不进入逐像素或逐帧合成；不新增失真的渲染 benchmark。
  以有界解码回归、标准条目样本和原生应用启动验证消费链。

## 产品资源

- `scripts/extract_windows2000_assets.py` 拥有 ISO 提取与产品转换；ISO、PE 和中间文件只在 `.local/`。
- `assets/classic/manifest.json` 拥有产品资源身份；`scripts/classic_assets.py` 验证并返回唯一安装清单，
  kernel、rootfs 与开发镜像同步均消费受 Git 管理的文件。缺失、损坏和未登记文件必须失败，不读取本地覆盖。
- 只发布实际消费者需要的原生尺寸和格式；新增资源必须在导入器登记 ISO 来源，不引入原创或替代资源回退。

## 字体目录与缓存

- typeface::FontCatalog 拥有扫描到的字体成员与 `fontmap.ini` 映射；Resources 每进程加载一次并共享，主题重载清空。
  轮廓字体经 linux-uapi `ReadOnlyMapping` 只读 `MAP_PRIVATE` 映射，同一 TTC 成员共享一份映射；页按需载入并跨进程共享页缓存，
  不再每进程整文件读入。字体文件只以 rename 替换，不得原地改写或截断（映射的 unsafe 前置条件）。
- OutlineFace 独占每个成员的 fpgm 结果；Typeface 独占本字号各成员的 prep 结果（按成员地址索引，chain 中的 Rc 保活）
  与已光栅化字形（上限 4096，满时按最近使用序号淘汰最旧四分之一）。缺少这些缓存会在每个字形重复执行字体程序；None 记录失败，避免反复执行。
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
  SHM 读取完成后即允许 release；失败返回截断文件或 no_memory（含 GPU 纹理建立/上传失败），不以空图像替代。
- damage 在 Pending 中按 surface/buffer 坐标累计，每个坐标系最多 16 个矩形，超出合并为外接矩形；commit 时换算为 buffer 像素，
  surface 损伤遇到 transform 或 viewport 退化为整幅。同尺寸且唯一持有的上一份内容只重读这些矩形，其余（首次、尺寸变化、
  被缓存提交或在用光标共享）整幅读取新快照；损伤之外沿用上一次提交，这是 Wayland 允许的语义。
  非同步提交先放开 current 的像素引用再读取，保证唯一持有；同步提交的 current 仍显示旧图，因此走写时复制。
- Committed.damage 合并同步缓存的多次提交，Surface.damage 合并已应用但尚未呈现的损伤；stage_frame 在成功合成后清空，
  flip 在途新提交的损伤留待下一帧。均使用原有 16 矩形预算；元数据提交不伪造像素损伤，null attach/尺寸变化由场景几何处理。
  opaque region 与 input region 一样按 commit 冻结；只采纳格式不透明或 region 可证明覆盖整块的情况，部分并集无法证明时保留绘制。
  XRGB/ARGB 切换不能沿用旧格式的局部快照，否则旧透明像素与新不透明保证冲突。
- Scanout.history 独占两个扫描目标的有效性、上一帧损伤和排序的 surface 身份/几何快照，不持有 Content/BufferUse。
  首次/启动画面覆盖后整幅修复；双缓冲复用修复本帧与上一帧损伤并集；绘制失败作废目标，有效性仅成功后发布。
  scene 比较包括层序、目标矩形与仿射源；移动/缩放/移除同时失效旧新位置，源损伤经逆变换向外取整。
  输出 Region 保持不相交，避免透明像素因重叠损伤被混合多次；最多 128 块，损伤碎片超限扩大为外接矩形，
  遮挡减法超限保持原区域，只增加绘制、不裁掉应显示内容。分割在现有 Vec 内复用，不逐矩形建立节点/暂存 Vec。
  CPU/GPU 共用自上而下的可见区域；GPU 以标准 VirGL scissor 裁片段，顶点/UV 保持原始目标，避免重新插值改变采样边界。
  scissor 按固定后端的正 viewport Y scale 转为 framebuffer 左下原点，u16 边界转换失败报错，不截断。
  背景使用一像素纹理按未覆盖区域替换，overlay 仅混合不相交修复区，关闭时释放其画布/纹理。
  未损伤的活 surface 保留采样视图，exec 只 pin 本帧实际使用的资源；不因局部帧反复重建视图。
  Frame.repair 保留同一次区域计算的目标修改集；CPU 绘制后、记录历史前通过标准 DIRTYFB 同步这些区域，随后再 page flip。
  传输的是相对该后缓冲的修复区，不能只传相对前台的 damage；首次 resident 的完整初始化由 adapter 保证。
  DIRTYFB 超过既有 32-clip 预算时合并外接矩形，坐标必须精确表示为 u16；空修复不发 DIRTYFB，错误使目标保持无效。
  沿用 adapter 已有的单次 synchronized 凭据，不新增可被 mmap 修改绕过的永久 clean 标记。
  Scanout.kms 在初始化时查询标准属性 ID，初次 modeset 与后续呈现均使用 MODE_ATOMIC，没有 legacy 回退。
  每帧 FB_DAMAGE_CLIPS 描述相对前台的输出差异；CPU DIRTYFB 仍描述该后缓冲的修复集，两者不能混同。
  原子请求和 clip 暂存跨帧复用容量；blob 创建者引用在提交后释放，pending/active 独立保活，语义由 [DRM 契约](drm.md)维护。
- Pixels 是 CPU 后端 SHM 的内存像素、GPU 后端 SHM 的纹理（像素只在其 guest backing），或 dma-buf BufferUse 与导入纹理；
  Content 只拥有共享像素引用与投影元数据；viewport 不再保存另一份 raw 内容。GPU 后端的 SHM 不保留 CPU 副本，硬件光标按需从 backing 复制。
  原地写 backing 前等待该资源上一次 transfer/exec 完成，避免旧帧混入新像素；已排队采样由主机命令顺序保证读到旧纹理。
  `VirglResource` 经 `&self` 只做拷贝式 read_row/write_row，不借出指向 backing 的引用，类型非 Send/Sync，故共享 Rc 的纹理可安全更新。
  null attach 使内容为空，取消 viewport 不能恢复旧缓冲。CPU/GPU 共用 ImageSource 三点仿射坐标与逻辑尺寸，
  按 transform、scale、viewport 顺序解释参数；裸 buffer 尺寸错误归属 wl_surface，viewport 源越界错误归属 wp_viewport。
- Subsurface.children 只索引存活关联，State.order 唯一拥有提交后的绘制顺序。创建、位置、堆叠在父提交后可见；
  销毁关联立即取消映射，保留永久角色，父销毁断开引用，编号重用不能复活关系。
  深度预算包含祖先和已有后代；超预算以标准 no_memory 失败，不能在绘制时悄悄漏掉深层节点。
- Region 以 COW 不可变快照实现复制语义；每对象最多 4096 条加减操作，超限标准 no_memory，防止绕过对象数量限制耗尽内存。
  hit_testing 按提交树逆序和输入 region 命中，空洞穿透；实际 leaf 收指针，根窗口收键盘/激活，隐式抓取保留 leaf 坐标原点。
- Session.pointer_grab 独占按下时的 leaf、serial 与物理位置；pressed_buttons 决定寿命，最后释放或目标消失时撤销。
  Seat 只签发/投递 serial，不保存第二份按钮授权。输入 Decoder 在按钮前提交已有轴值，Button 携带事件位置；
  同一批次之后的 motion 不能反向改变点击命中、菜单选择或抓取锚点。sync_focus 也保留仍存活的隐式目标。
  xdg_toplevel.move/resize 校验该 serial 和根目标，以按下位置作锚点；请求到达时立即应用已发生的位移，
  无须等待下一个 motion。语义遵循固定 xdg-shell 用户事件授权及
  [Mutter 49.0 的 grab 坐标捕获](https://github.com/GNOME/mutter/blob/49.0/src/wayland/meta-wayland-pointer.c)。
  只扩展既有抓取记录和语义事件，没有新锁、分配或全局状态；成本沿用 GPU/显示门禁。
  实际 classic-ui 探针故意在 motion 后发送 move/resize，原位置与尺寸像素断言裁决延迟交错；
  Decoder host 测试保护同批轴值→按钮→后续轴值的顺序边界。
- Stack 从活动窗口的父链推导全屏组；该组及其 popup 位于 Top 外壳之上、Overlay 之下，绘制与命中共用同一分界。
  非全屏窗口激活后恢复普通层序；全屏对话框保持所属组，不另存层级状态。成本为栈/父链遍历，无新增逐帧分配，交互由桌面验收验证。
- queued_callbacks 等待显示提交，in_flight_callbacks 只属于已送交 Scanout 的帧；finish_frame 不能消费 flip 在途期间新提交的回调。
  presentation 显式登记可见树和实际呈现的客户端硬件光标，隐藏 surface 不提前获得下一帧许可。
- GPU.overlay 缓存当前合成器叠加层边界大小的画布与纹理；关闭释放。移动时复用同尺寸存储，重画局部画布后上传连续像素。
  SHM 内容与叠加层的采样纹理只保留最小 GEM 后备，大块上传共用 VirGL context 的一块暂存。
  暂存由 context 克隆共同持有，容量随最大上传区域增长；重用前检查其最后 fence，忙时换新并由内核提交 pin 保活旧存储。
  缓存同时记录最近一次上传时间。Session 将一秒空闲期限并入原 poll：期限前继续复用，期限后仅当
  最后 fence 已完成才清掉暂存；设备仍忙时推迟重试，不阻塞 GPU。缓存为空后撤去该唤醒。
  这是 Skia 式 deferred scratch cleanup，释放不再使用的上传副本，保留纹理内容、在途命令和连续视频/缩放的复用。
  小区域沿用有序内联写入；CPU 光标等消费者按需建立独立读回暂存与像素快照。
  Benchmark 决策为在现有来宾比较窗口首次绘制、交互缩放和持续局部更新的延迟及内存，确认共用暂存的分配和内联写入成本。
  四边形仍在输出坐标中定位；CPU 后端直接画进扫描缓冲。成本由实际输出下的菜单驻留和交互帧诊断，沿用 flip 与像素门禁。
- GPU.views 以真实资源编号与采样格式为键；VirGL 对象句柄从固定状态句柄之后独立分配，销毁命令先于句柄复用。
  上一帧 flip 完成后才能淘汰视图/覆盖顶点，exec 成功后才发布新缓存。没有逐四边形重复资源表或每帧重复创建采样视图。
- Session 的 applied_client_cursor 只保存成功上传的不可变图像引用及物理热点；同图像/热点跳过上传，形状切换或隐藏清空，
  位置仍独立更新。set_cursor 校验本连接最近 enter 序号后才分配角色，后续 offset 从设置时热点扣除累计增量。
- Benchmark 决策：移除 viewport 全图重采样、窗口元数据查询的子树分配、逐帧采样视图创建与未变化光标上传；
  CPU 仿射路径只扫描可见目标像素，不分配中间图像。相同 set_text 不使布局失效。
  SHM 提交的成本从“整幅 pread + 整幅 CPU 副本 + 新建纹理 + 整幅上传”降为 O(损伤像素)，复用纹理与采样视图；
  由 shm 子矩形读取与损伤/写时复制单元测试守护语义，不新增宿主墙钟阈值。
  验证范围见[Wayland 验证](../development/wayland-ui-verification.md)，不声明已提高多少帧率；原生控件仍走 wl_shm，opaque 只用于已证明安全的遮挡剔除。

## 键盘对象与迟绑定

- Seat 唯一拥有 logical keyboard focus、pressed keys 与 modifiers；Client 的每个 wl_keyboard 只保存是否已投递 enter 的 wire 状态。
  新资源先取得 keymap/repeat_info；即使 logical focus 未变化，也必须由 Seat 用当前真实状态补发 enter/modifiers。
  已 enter 的旧资源不能重复 enter，尚未 enter 的资源不接收按键；release 删除资源，surface 先销毁时只清理 wire 状态、不发送失效引用。
- `Server::keyboard_needs_enter` 只读检查资源状态，不 flush socket、不维护另一份焦点或按键表；正常焦点改变仍先 leave 再 enter。
- OnDemand 图层接受指针点选取得键盘焦点。独占图层只暂借焦点，任务栏提交最小化动画矩形只更新几何。
  成本决策：无新锁、缓存或逐帧分配，查询为现有连接/对象表的有界只读遍历；沿用控件/Wayland 门禁，以 wire 顺序和迟绑定输入验证，不另设 wall-clock 阈值。

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
