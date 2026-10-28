# 本机登录与桌面

产品入口为 `display-service → compositor → logon → 用户 desktop`。
`logon` 以初始受信任 Wayland 连接常驻；desktop 获得独立的 security-context Shell 连接。
窗口管理仍由 compositor 拥有，账户与会话生命周期由 logon 拥有。

## 首次运行

1. `make build-rootfs` 构建产品，`make run-gui` 启动图形实例；旧开发镜像先运行 `make sync-userland`。
2. 默认用户名 `root` 没有口令：开机直接进入桌面，Ctrl+Alt+Delete 的锁定与解锁也不再弹出登录对话框。
   可在 UART 恢复 shell 执行 `passwd root` 设置口令，或执行 `adduser <用户名>` 创建普通账户；nobody 保持锁定。
3. 账户有口令时，图形界面按 Ctrl+Alt+Delete，输入用户名与口令。登录后可通过同一序列锁定、注销、改密或关机。
   用户主目录必须存在；会话创建用户拥有的 0700 `/run/user/<uid>`。

`sync-userland` 安装账户工具；缺少 shadow 时创建空密码 root 默认，完整匹配旧出厂锁定模板时迁移到新默认，其他已有密码、自定义账户与主目录不被替换。
密码变更先验证当前密码与 shadow 最短改密间隔（UID 0 保留管理员例外），再调用固定 BusyBox passwd 进行带随机 salt 的 SHA-512 crypt 更新与文件锁定/替换。

## 当前可用范围

- 欢迎页（登录类对话框宽度与横幅位图一致）、SAS、密码登录/错误反馈、加载交接、安全页、锁定/解锁、更改密码、注销、关机与重启。
- 蓝色桌面、三个图标、28 像素任务栏、应用按钮分页、显示桌面、我的文档、UTC 时钟、ALSA 音量与静音。
- 托盘喇叭单击弹出贴着任务栏的小音量面板（滑块与静音，点外部或 Esc 关闭）；右键喇叭选"调整音频属性"才启动完整的 `sndvol` 窗口。
- 音量控制 `/bin/sndvol`（单列混音器：平衡、音量、全部静音；控制面板"声音和多媒体"的"声音"页可试听系统事件声音，"音频"页显示播放设备并打开它）与"关于 LiteOS"（`control-panel winver`）同样是独立程序；
  托盘小面板与它共用 `sndvol` 库的状态和布局。
- 任务管理器是独立程序 `/bin/taskmgr`，提供应用程序、进程、性能与 GPU 四页；开始菜单、任务栏右键、Ctrl+Shift+Esc 与安全界面都只是启动它。
  应用列表走 foreign-toplevel，进程与系统总量读 procfs；CPU 使用记录可切换总图/每个逻辑 CPU 一图，内存按量级显示单位，GPU 仅显示可读取的 DRM 统计。
- 桌面显示三个系统图标加 `~/Desktop` 里的条目（文件夹、文本、程序、其他文件），目录变化时自动刷新；图标可拖动并吸附网格，位置保存在 `~/.config/liteos/desktop-icons`；右键空白处可"排列图标"（清除保存位置）、刷新、新建文件夹。
- 双击托盘时钟打开"日期/时间 属性"，按 timedate.cpl 对话框还原：“时间和日期”页有月份下拉、年份微调、CalWnd 月历、
  指针时钟与 hh:mm:ss 微调时间框，底部为当前时区；“时区”页为 Win2000 的 74 个时区、世界地图与“根据夏时制自动调节时钟”。
  时区写成 TZif v2 加 POSIX 规则（含夏令时切换）。默认 `/etc/localtime` 为北京时间（UTC+8，CST）；
  rootfs 指纹包含该文件，开发镜像同步保留用户已有选择。地图不按选中时区高亮调色板区域。控制面板另有"系统属性"（常规/网络标识/硬件三页：系统与版本、注册到、处理器与内存；
  计算机全名；设备管理器入口。原版的用户配置文件、高级页没有对应功能，不画占位）。
- 列表表头（任务管理器进程页、资源管理器“详细信息”）的列分隔线可拖动调整列宽，指针在分隔线上显示左右箭头（`wp_cursor_shape_v1` 的 `col_resize`）；状态栏面板宽度由文字量出，超出时截断成“...”，
  所有由数据或语言决定长度的文字统一走 `Painter::fit`/`text_fit`/`label_fit`。
- 鼠标滚轮：compositor 读 `REL_WHEEL`/`REL_WHEEL_HI_RES`，经 `wl_pointer.axis_discrete`/`axis`/`frame` 发给指针下的窗口，classic-ui 汇总为 `EventKind::Wheel`（档位，每档 3 行）；资源管理器（树与列表）、记事本、任务管理器、
  设备管理器、文件对话框、显示属性的颜色列表都支持。任务按钮带程序图标；资源管理器的文件、文件夹与磁盘"属性"是独立窗口（`control-panel properties`，可拖动，含大小、占用空间、时间戳、只读属性与磁盘容量饼图）。
- 右键菜单：任务栏空白处（最小化所有窗口、任务管理器）、任务按钮（还原/最小化/最大化/关闭）、桌面图标（打开）、桌面空白处（刷新）。
  层叠/平铺窗口、排列图标、属性等需要窗口摆放协议或设置程序的项不提供，也不画灰色占位。任务按钮翻页箭头只在放不下时出现。
- 开始菜单是 Windows 2000 经典布局（程序/文档/设置子菜单、运行、锁定、注销、关机），支持鼠标、方向键、Enter、Escape，点击外部关闭；Ctrl+Esc 开始，Super+R 运行。
- 运行以当前用户执行 shell 命令，是外壳的 layer-shell 窗口（Top 层、锚定左下角、独占区 0 因而避开任务栏，可拖动标题栏移动，打开即获得键盘）；普通图形应用从公开 Wayland socket 连接，不能访问外壳权限。
- 任务按钮激活/恢复和最小化，应用任务窗口请求正常关闭；资源管理器（见[设计](../design/classic-desktop/explorer.md)）：
  ReBar 三行带区（菜单栏、标准按钮与工具提示、地址下拉与“转到”）与转动的品牌 logo、级联子菜单；
  Web 视图面板（窄窗口时隐藏）、各位置的详细资料列、“我的电脑”容量与饼图；
  回收站原位置/删除日期与清空/全部恢复/还原、网络和拨号连接、“浏览文件夹”对话框、三格状态栏与尺寸手柄。
- Desktop Entry 读取系统安装目录，支持 Application/Name[zh_CN]/Exec 与 Hidden/NoDisplay/Terminal；Exec 分词后直接 spawn，禁止 shell 展开。

空 shadow 字段只接受空输入且不弹出对话框，`!`/`*` 锁定值仍拒绝。散列认证仅支持本机 SHA-256/512 crypt，锁定、账户过期、密码过期与强制改密的账户拒绝图形登录；没有 PAM、远程域或拨号。
解锁接受原 UID 或本机 UID 0 管理员，其他普通账户不能接管既有会话；管理员解锁不改变原桌面的执行身份。
Terminal=true 桌面条目当前不显示；终端应用的使用与待验收范围见 [SDK 开发](ui-sdk.md#系统内终端)。
资源管理器的工具栏有悬停提示（停留 500 毫秒），单按 Alt 进入菜单栏键盘选择（左右移动、Enter/向下打开），Alt+字母直接打开对应菜单，助记键下划线按 Alt/Tab 之后才显示。
共享控件：`classic-ui` 提供菜单栏状态机 `MenuBar`（鼠标、方向键、Alt 激活、Alt+字母直达、按过 Alt/Tab 后才显示助记键下划线；任务管理器、记事本、媒体播放机、音量控制共用）、
文件打开/另存为对话框 `FileDialog`（记事本、媒体播放机）、单行输入框 `Prompt`（任务管理器的新任务）。
剪贴板：compositor 公布 `wl_data_device_manager` v3 的选择部分（只有文本，没有拖放）；只有键盘焦点所在的程序能设置选择，内容经管道在两个程序之间直接传递。记事本的剪切/复制/粘贴使用它。
资源管理器支持多选：Ctrl 点击切换、Shift 点击选区间、Ctrl+A 全选，复制、剪切、删除、粘贴作用于全部选中项；重命名与属性只作用于焦点项。
窗口最小化与还原、最大化与还原有 150 毫秒的整窗缩放动画（标题栏、客户区与子 surface 同步）；最小化目标：任务栏经 `zwlr_foreign_toplevel_handle_v1.set_rectangle` 告知任务按钮位置；没有位置时取屏幕底边中部。
桌面设置：桌面右键或任务栏右键的“属性”、开始菜单“设置”打开控制面板的“显示”“任务栏”，写入 `~/.config/liteos/desktop.conf`；桌面外壳每个周期检查它的修改时间，
变化后重配任务栏图层（总在最前=Top/Bottom 层，自动隐藏=释放独占区并收成细线、指针进入展开、离开 400 毫秒后收起）并重画背景色与时钟。任务栏时钟、音量与任务按钮悬停约 0.5 秒显示工具提示（独立的小图层）。
桌面图标右键：文件与文件夹有打开、删除、重命名、属性——后三项由桌面启动 `file-explorer <文件夹> --select <名称> --verb <delete|rename|properties>`，资源管理器打开所在文件夹、选中它并立即走自己的确认框、
原地改名或属性对话框，结束后窗口自动关闭；“我的电脑”有属性（系统属性）。
设备管理器 `/bin/devmgmt`：控制面板“系统→硬件”的按钮启动，按类别列出能从 `/proc/stat`、`/sys/block`、`/dev/dri`、`/dev/snd`、`/dev/input`（evdev 设备名）、`/proc/net/dev` 确认的硬件，双击看属性；内核没有统一的设备枚举，所以不全。只读。
网络：控制面板、开始菜单“设置”与“网上邻居”属性打开“网络和拨号连接”文件夹，按 `/sys/class/net` 列出“本地连接”。
  右键为 netshell 的禁用/启用、状态、属性；双击打开“本地连接 状态”（只有“关闭”，内含“属性”“禁用”）。
  “本地连接 属性”（确定/取消）列出 Internet 协议 (TCP/IP)，其“属性”打开 TCP/IP 对话框：自动获得或指定 IP、掩码、网关与 DNS。
  设置写入 `/etc/network/interfaces`（interfaces(5) 子集），network-service 重新配置：DHCP 由受监督的 udhcpc 完成，
  静态地址用 ifconfig/route，DNS 写 `/etc/resolv.conf`，禁用即去掉 auto 并关闭接口。重命名、快捷方式与“高级”TCP/IP 设置禁用。
网络与设备管理器、记事本使用从原版 `netshell.dll`/`devmgr.dll`/`notepad.exe` 提取的专用图标，随受 Git 管理的资源集安装，来源见[资源清单](../design/classic-desktop/assets.md)。
开始菜单程序项的图标来自各桌面条目的 `Icon=`，缺素材时退回通用图标。
文件关联：桌面条目的 `MimeType=` 与 `Exec` 的 `%f` 决定谁打开文件，扩展名到类型的映射在 `desktop-entry`；`.wav` 交给媒体播放机 `/bin/mplay`，常见文本扩展名交给记事本 `/bin/notepad`，资源管理器与桌面图标共用这套查找。
资源管理器单目录最多 4096 项；不编辑文件、不预览内容。
资源管理器的删除确认框与文件操作失败提示框（红色错误图标、“确定”）会播放 `chord.wav`；复制、重命名等失败原因不再写入状态栏。其余系统事件声音（如警告、信息框）尚无调用者。
任务窗口只请求应用正常关闭，不能强制结束无响应程序；输入法与完整任务栏属性尚未提供。默认音频节点为 0600，音量仅对有设备权限的账户可用，无 GUI 提权路径。
会话进程组监督覆盖正常启动的应用，主动脱离进程组的程序需要后续 cgroup 会话资源域。
这些范围不代表完整 Windows 2000 shell 或所有经典桌面交互规格均已完成。

## 验收边界

| 范围 | 当前边界 | 验收依据 |
|---|---|---|
| 外观与 HiDPI | 原版资源与整数缩放已接入，完整应用视觉验收尚未完成 | 按[设计规格](../design/classic-desktop/README.md)逐项核对 1x/2x 的字体、图标、光标、控件与裁剪；待核数值不自动提升置信度 |
| 外壳交互 | 设计清单仍含未实现或未验收行为 | 按[交互清单](../design/classic-desktop/interaction.md)及[外壳规格](../design/classic-desktop/shell.md)核对 XOR 拖动、菜单动画、个性化菜单、拖动排序、气球提示与系统事件声音 |
| 输入与文件操作 | 已有文本选择传输，未实现标准文件拖放与 IME | 协议支持范围由[设备架构](../architecture/devices-terminal.md)拥有，不以文本剪贴板代替文件拖放或输入法 |
| 媒体播放 | 当前只播放 16 位 PCM WAV | 压缩音频解码、在线来源与录音尚未实现，不把播放窗口存在视为完整媒体播放器 |

共享框架的生命周期、GPU 缓冲与性能证明范围见[Wayland 验证](wayland-ui-verification.md)；
Win32 conformance、红警 2 与 D3D 的未完成范围见 [Wine 验证边界](wine-runtime.md#conformance-对照与游戏验收)。

## 门禁

`make verify-runtime-desktop` 使用真实产品 logon/desktop/file-explorer，不用 UI 探针代替产品。
它向一次性镜像注入 alice/bob 与 root 的测试散列（产品 root 无口令会自动登录，不能走密码流程）、procfs 检查脚本与独立标准 ALSA C observer，绝不写入产品或开发账户。
冷启动 topology 复用 display_session 的 AArch64/HVF、2 CPU、1280×800，并通过共用 QEMU owner 添加 VirtIO Sound。

1. 欢迎、SAS、错误密码拒绝与正确密码；背景图标、任务栏像素和两个实际首帧 callback。
2. procfs 四个 UID/GID、补充组集合与运行时目录的真实 owner/mode。
3. 图标启动资源管理器（主目录显示为"我的文档"）、键盘选择、任务按钮最小化与恢复，开始菜单"运行"启动真实后台进程。
4. 验证更改密码后旧密码拒绝、新密码可解锁；普通其他账户拒绝解锁、UID 0 解锁保留 alice 会话；注销清空图层并终止后台组成员。
5. 托盘面板单击弹出与 Esc 关闭、右键菜单打开音量窗口；另用产品原样镜像验证无口令 root 开机直接进入桌面、锁定后解锁都不出现登录对话框。
6. 无权限账户打开音量窗口保持桌面正常；root 登录后拖动 slider、切换静音，独立 C 程序直接用 Linux headers 和 ioctl 读回真实 Master 值。

Wayland/控件协议回归仍由 [Wayland 门禁](wayland-ui-verification.md) 独立裁决。
首帧接管测试在一次性镜像中暂时不启动 logon，以逐像素验证交接画面；产品欢迎与完整会话由本门禁覆盖。
完整 `make verify` 串行包含这两个门禁；定向开发时不重复全量单测。

## 成本与安全所有权

- 没有内核热路径、锁或新 syscall。桌面按配置、输入、窗口列表变化和分钟更新合并失效窗口，每窗口最多一条在途 frame。
- ALSA UI 投影按 100ms deadline 读取，仅实际变化合并重绘；操作使用现有 control ioctl，无新增内核锁/热路径。布局由固定 Linux headers 的 C static assertions 与真实 UI 读回验证，不增加宿主 wall-clock 阈值。
- BMP V4 素材在启动期读取/校验并预乘 alpha，每帧复用；解码器有截断、大小和 alpha 测试，无新增独立 benchmark 阈值。
- 认证 crypt_r 是交互低频、由固定 musl 实现；错误后至少一秒重试窗口，不缓存密码。当前分配内的明文（含已删除尾部）在提交/取消/切换后 volatile 清空，不声称擦除此前已释放的旧分配。
- SAS 期间仅可信 Overlay 的来源优先级与输入屏障新增到 compositor 策略，不改变普通窗口布局或通用 kernel ownership。
- 光栅成本沿用 canvas/typeface 诊断 benchmark；交互门禁验证帧节流与更新正确性，不把截图正确性宣称为性能证明。
