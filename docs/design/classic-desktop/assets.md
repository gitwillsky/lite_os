# 素材清单

仓库内位图素材由 Codex 按本文件的要求生成（流程见 `.claude/skills/generate-assets-with-codex`），源 PNG 放在
`assets/classic/<类别>-src/`，再由各自的打包步骤转成消费格式。文件名是契约：打包脚本与代码按名字取图。

## 本地原版素材

用户已选择保留 LiteOS 界面文案、优先使用自己的原版图像、声音和字体。提取 owner 是
`scripts/extract_windows2000_assets.py`，输出仅位于 Git 忽略的 `.local/inputs/windows2000/assets/`。
仓库中的原创素材和替代字体保留。`classic_assets` 统一选择本地原版优先的输入，启动素材与 rootfs 安装消费同一选择；
提取目录经 manifest 校验后显式打包。操作见
[构建与验证](../../development/build-and-verify.md#经典桌面本地素材)。

原版输入是用户提供的 `Windows 2000 Professional Service Pack 4 CHS.iso`，402626560 字节，SHA256：
`a8e8d656bd147700c8c73e2ef0a47de7832b6c430d442f539f731db180ea8cb0`。
这是本地文件的内容标识，不声称已认证发行方。PE 资源优先级不猜测：下面的映射明确选取简中资源（语言 2052），
保留原始图像数据与掩码，按资源目录重建 ICO/CUR/BMP 容器，并生成原生 16/32/64 尺寸中实际存在的 PNG；
不虚构 64 像素原图。ICO/CUR 只写入目录引用的图像数据，不发布提取工具附带的未引用内存尾部。

| 逻辑名 | 来源与资源名/号（不是零起始 icon index） |
|---|---|
| `my-computer` / `my-documents` | `shell32.dll:16` / `mydocs.dll:100` |
| `recycle-bin-empty` / `recycle-bin-full` | `shell32.dll:32` / `shell32.dll:33` |
| `folder-closed` / `folder-open` | `shell32.dll:4` / `shell32.dll:5` |
| `file-generic` / `file-text` / `file-app` | `shell32.dll:1` / `152` / `3` |
| `drive-hdd` / `drive-cd` / `control-panel` | `shell32.dll:9` / `12` / `137` |
| `drive-floppy` / `drive-network` / `printers` / `file-system` | `shell32.dll:6` / `10` / `138` / `153` |
| `programs` / `documents` / `settings` | `shell32.dll:20` / `21` / `22` |
| `search` / `help` / `run` | `shell32.dll:23` / `24` / `25` |
| `shutdown` / `logoff` / `lock` / `key` | `shell32.dll:28` / `45` / `48` / `194` |
| `command-prompt` / `task-manager` / `media-player` | `cmd.exe:IDI_APPICON` / `taskmgr.exe:107` / `mplayer2.exe:110` |
| `show-desktop` / `display-properties` / `date-time` | `explorer.exe:103` / `desk.cpl:40` / `timedate.cpl:200` |
| `device-manager` / `network-connections` / `network-local` / `network-disconnected` / `network-places` / `notepad` | `devmgr.dll:201` / `netshell.dll:10010` / `netshell.dll:190` / `netshell.dll:195` / `shell32.dll:18` / `notepad.exe:1` |
| `sound-volume` / `users` | `sndvol32.exe:304` / `netplwiz.dll:102` |
| `tray-volume` / `tray-volume-muted` | `stobject.dll:230` / `231`（托盘只有 16×16 原生图） |
| `msg-error` / `msg-warning` / `msg-info` / `msg-question` | `user32.dll:103` / `101` / `104` / `102` |
| `ctrl-alt-del` | `msgina.dll:109` 图标（键盘）；按原生尺寸保留，不冒充 48×48 位图 |
| `boot-scene` | `ntoskrnl.exe:1`，640×480 **完整启动画面**，不是下方原创的中央 `boot-logo` |
| `logon-banner` / `start-banner` | `msgina.dll:101`（413×72）/ `explorer.exe:157`（21×233） |
| `toolbar-default` / `-hot` / `-disabled` | `browseui.dll:272` / `273` / `276`（940×20，47 帧，洋红 `#FF00FF` 为透明色，提取时转 alpha 0） |
| `ie-toolbar-default` / `-hot` / `-disabled` | `browseui.dll:261` / `262` / `265`（320×20，16 帧；后退 0、前进 1、搜索 5、历史 12） |
| `go-default` / `go-hot` | `browseui.dll:290` / `291`（地址栏“转到”箭头，单帧） |
| `brand-22` / `brand-26` / `brand-38` | `browseui.dll:306` / `305` / `304`（品牌 logo，正方形帧竖排，首帧静止，其余为忙碌动画） |
| `worldmap` | `timedate.cpl:50`（356×184 下标图）；时区区域来自 `HIVESFT.INF` 的 `Time Zones\MapID` |
| `webview-left` / `webview-line` | `webvw.dll` 类型 23 `WVLEFT.BMP`（127×56 水印）/ `WVLINE.GIF`（152×2 分隔线） |
| `entire-network` / `desktop` | `shell32.dll:14`（整个网络）/ `shell32.dll:35`（桌面） |

`liteos-logo` 沿用原创品牌图标。原版位图中烘焙的品牌文字不修改；动态文案始终为 LiteOS。
仓库只提交原创默认素材；工具栏位图条、品牌 logo、Web 视图位图与上表中新增的外壳图标只有本地原版，`shell_bitmaps()` 取两个目录按文件名的并集，缺失时界面不画该图标。

地图提取与打包保留原始 8-bit 调色板下标，使用标准 top-down BMP V4（BI_RGB、RGBQUAD 调色板）；
其他系统图像沿用 32-bit BI_BITFIELDS RGBA。共享 Bitmap 解码并保留不可变下标，Image 控件更换调色板时生成像素，
不逐帧重着色。控制面板只提供蓝海绿陆及选中时区加亮的颜色表；颜色是用户选择，不声称还原原版运行时调色板。

原版默认光标来源是 `user32.dll` 的 group cursor：`arrow:100`、`text:101`、`busy:102`、
`precision:103`、`resize-ns:104`、`resize-nwse:105`、`resize-nesw:106`、`resize-ew:107`、
`move:109`、`unavailable:110`、`working:111`、`help:112`、`link:114`。热点读取 CUR 文件目录，
不沿用下方原创光标的估计值。CUR 保留 AND/XOR 掩码；普通 RGBA 不能表示文本光标等的背景反色。
安装介质中的 `.ani` 也原样保留，帧序和速率不得强制替换成原创素材的 8 帧/10 fps。

原版声音映射依据 `I386/HIVEDEF.INF` 的默认 AppEvents：`logon → logon.wav`，
`critical`、`exclamation`、`asterisk`、`question → chord.wav`；另提供 `logoff → logoff.wav`。
原始文件均为 22050 Hz、16 bit、stereo PCM，保留采样率与长度，不按下方原创声音规格转码。
原版字体为 `simsun.ttc`（保留点阵表和 collection）、`tahoma.ttf`、`tahomabd.ttf`，运行时作为数据加载。
该 collection 含 SimSun、NSimSun 两个 face，均有 12/13/14/15/16/18 ppem 点阵；不抽取轮廓而丢失 1x 点阵。

`manifest.json` 记录输入 ISO 标识、工具版本、每个产物的字节数/SHA256、逻辑名、资源号、原生尺寸、
光标热点及声音参数。所有导出图像帧必须能解码；工具的非致命警告也写入 manifest，不因退出码为零而隐藏。
这份介质的 `main.cpl` 有一处 icon group 长度不一致警告，导出的所有图像帧已实际解码通过。

以下章节只定义仓库原创素材与替代字体。

## 通用要求

1. 风格：Windows 2000 经典风格——等距略带立体感的 256 色观感、清晰的 1 像素黑色或深色轮廓、
   左上方光源、不使用 Windows XP 式的圆润高光与大面积渐变。
2. 原创：不得复制或描摹任何微软图标、logo 与位图；只还原风格。
3. 格式：RGBA PNG，透明背景，直 alpha（打包到预乘合成器时由打包脚本预乘）。
4. 倍率：1x 是权威尺寸。素材可以缺少 2x（原版素材没有），缺失时使用方对 1x 做**整数**最近邻放大，不另存放大图，
   也不做非整数缩放（HiDPI 规则见 [appearance.md](appearance.md)）。仓库内已有的 64 像素图标与光标是原创的 2x 重绘，
   存在时优先于放大。
5. 小尺寸可读性：16 × 16 图标必须在 1x 下清晰可辨，只保留最主要的形状。

## 图标（`assets/classic/icons-src/`）

每个图标三张：`<名称>-16.png`（小图标 1x）、`<名称>-32.png`（大图标 1x，同时用作小图标 2x）、`<名称>-64.png`（大图标 2x）。

| 名称 | 语义 |
|---|---|
| `liteos-logo` | LiteOS 标志，用于"开始"按钮与关于对话框；原创图形，四色块构图不得与 Windows 旗帜相似 |
| `my-computer` | 我的电脑：米色 CRT 显示器与主机 |
| `my-documents` | 我的文档：黄色文件夹上放一张写有文字的白纸 |
| `recycle-bin-empty` | 回收站（空）：带网格纹理的空废纸篓 |
| `recycle-bin-full` | 回收站（满）：同一废纸篓装满揉皱的纸 |
| `folder-closed` | 关闭的黄色文件夹 |
| `folder-open` | 打开的黄色文件夹 |
| `file-generic` | 未知类型文件：右上折角的白色文档 |
| `file-text` | 文本文件：带横线文字的白色文档 |
| `file-app` | 可执行程序：带标题栏的小窗口 |
| `drive-hdd` | 本地磁盘：灰色硬盘侧视图带绿色指示灯 |
| `drive-cd` | 光盘驱动器 |
| `control-panel` | 控制面板：文件夹前放着螺丝刀与小屏幕 |
| `command-prompt` | 命令提示符：黑色窗口内白色 `C:\>` 字样 |
| `task-manager` | 任务管理器：带折线图的小窗口 |
| `media-player` | 媒体播放器：播放三角与音符 |
| `programs` | 开始菜单"程序"：一组叠放的程序窗口 |
| `documents` | 开始菜单"文档"：叠放的文档 |
| `settings` | 开始菜单"设置"：带复选框的设置面板 |
| `search` | 开始菜单"搜索"：放大镜 |
| `help` | 开始菜单"帮助"：蓝色问号书本 |
| `run` | 开始菜单"运行"：窗口与指向它的箭头 |
| `shutdown` | 开始菜单"关机"：电源按钮 |
| `logoff` | 注销：钥匙与门 |
| `lock` | 锁定计算机：挂锁 |
| `show-desktop` | 快速启动"显示桌面"：桌面上放着一支铅笔与吸墨纸 |
| `display-properties` | 显示属性：带调色板的显示器 |
| `date-time` | 日期/时间：日历与时钟 |
| `sound-volume` | 声音：喇叭 |
| `users` | 用户和密码：两个人像 |
| `key` | 登录：金色钥匙 |
| `ctrl-alt-del` | "欢迎使用"对话框左侧的键盘图示（原版 `msgina.dll:109`，32 × 32）；只有 `-32.png` 与 `-64.png` |

托盘专用（只有 `-16.png` 与 `-32.png` 两张，分别对应 1x 与 2x）：

| 名称 | 语义 |
|---|---|
| `tray-volume` | 托盘音量：小喇叭 |
| `tray-volume-muted` | 托盘音量静音：小喇叭带红色禁止圆圈 |

消息框图标（只有 `-32.png` 与 `-64.png`）：

| 名称 | 语义 |
|---|---|
| `msg-error` | 红色圆形内白色叉 |
| `msg-warning` | 黄色三角形内黑色感叹号 |
| `msg-info` | 白色气泡内蓝色小写 i |
| `msg-question` | 白色气泡内蓝色问号 |

## 光标（`assets/classic/cursors-src/`）

每个光标由 1 到 N 帧组成：`<名称>-32.png`（单帧）或 `<名称>-<n>-32.png`（多帧，n 从 0 起连续），以及对应的 `-64.png`
（2x，硬件光标上限 64 × 64）。帧数、帧率与热点**随素材包提供，不是代码常量**：原版默认方案的箭头热点在 (0, 0)、
文本光标在 (15, 16)，各光标不同，同一份代码必须都能用。光标阴影由 compositor 绘制
（见 [interaction.md](interaction.md) E4），素材不自带。

| 名称 | 语义 | 仓库原创素材的热点（1x） |
|---|---|---|
| `arrow` | 标准左上箭头 | (0, 0) |
| `help` | 箭头右侧带问号 | (0, 0) |
| `text` | I 形文本光标 | (16, 16) |
| `precision` | 细十字准星 | (16, 16) |
| `unavailable` | 圆圈加斜杠 | (16, 16) |
| `resize-ns` / `resize-ew` / `resize-nwse` / `resize-nesw` | 四个方向的双向箭头 | (16, 16) |
| `move` | 四向箭头 | (16, 16) |
| `link` | 伸出食指的手 | (6, 0)，食指指尖 |
| `busy` | 沙漏（原创素材 8 帧、10 fps；原版 Windows 2000 默认是静态单帧） | (16, 16) |
| `working` | 箭头加小沙漏（后台运行；同上） | (0, 0) |

本地原版（`pack_cursors.py --original`）取安装介质 I386 里默认方案的 `*_r.cur`（`arrow_r`、`beam_r`、`busy_r`、`wait_r`、
`cross_r`、`no_r`、`move_r`、`help_r`、`size1_r`..`size4_r`），热点读自这些文件的目录项；`user32.dll` 导出的同名资源的热点字段
全是 (10, 10)，不可信，不使用。原版只有 32 × 32，2x 为整数最近邻放大。这些光标没有"反色"像素，AND/XOR 可无损转成 RGBA；
遇到反色像素打包即失败。默认方案没有手形光标，`link` 沿用仓库原创。

## 位图（`assets/classic/bitmaps-src/`）

只有 1x：`<名称>-1x.png`，不透明 RGB。尺寸由原版决定，布局代码按位图实际尺寸排版，所以仓库原创素材必须与原版尺寸一致。

| 名称 | 尺寸 | 语义 |
|---|---|---|
| `boot-scene` | 640 × 480 | 整幅启动画面，打包成 `LBS1`（见 `scripts/pack_boot_scene.py`）后由内核与 compositor 共用。几何见 [boot-logon.md](boot-logon.md) |
| `logon-banner` | 413 × 72 | 登录类对话框顶部横幅：左侧 logo，右侧产品名，浅灰到白的水平渐变，底部 1 像素深色线 |
| `start-banner` | 21 × 233 | 开始菜单左侧竖条：上黑下蓝的竖向渐变，白色文字自下而上竖排，靠底部对齐；菜单更高时底部对齐，上方用素材顶部颜色填充 |

## 字体（`assets/fonts/`）

映射表 `fontmap.ini` 与下列字体一起安装到 `/usr/share/liteos/fonts`；本地原版同名文件优先并加入 `simsun.ttc`。

| 用途 | 字号 | 文件 | 许可证文件 | 置信度 |
|---|---|---|---|---|
| 宋体的可分发替代（12 像素点阵） | 12 ppem | `wenquanyi_9pt.bdf`：文泉驿点阵宋体 12 像素点阵，30503 字 | `WenQuanYi-LICENSE`（GPL-2.0 + 字体嵌入例外） | 确定 |
| 宋体的可分发替代（轮廓） | 其余 ppem | `ar-pl-uming-cn.ttf`：文鼎 PL 明体 CN 轮廓，24232 个码位 | `ARPHICPL.TXT`（Arphic Public License，原样保留） | 确定 |
| Tahoma 的可分发替代 | 全部 | `tahoma.ttf`、`tahomabd.ttf`（Wine 11.16 由 `tahoma.sfd`、`tahomabd.sfd` 构建） | `Wine-Tahoma-LICENSE`（LGPL-2.1+） | 确定 |
| 等宽字体（开机文本、命令提示符） | 全部 | 现有 `terminal-font`，整数放大 | 仓库已有 | 确定 |

来源（均为上游原始包，不含发行版补丁）：

| 字体 | 来源包 | SHA256 |
|---|---|---|
| 文泉驿点阵宋体 | Debian `xfonts-wqy_1.0.0~rc1.orig.tar.gz` | `af655fedfb490aabbb8ce73602a08a6a847fafc78dc363ff3901aa6c3384ca5d` |
| 文鼎 PL 明体 | Debian `fonts-arphic-uming_0.2.20080216.2.orig.tar.bz2` | `e3c19e04ea7a565b4acff6f1e4248084d2e10752e305bf7dd6c76e80860dc1db` |
| Wine Tahoma | `wine-11.16.tar.xz` | `c66e2090343dcd727f7f7fd2f87ee0bfb0b118790c1d745ab7b8a4c3a4197f2f` |

`ar-pl-uming-cn.ttf` 是修改版：从 `uming.ttc` 抽出 "AR PL UMing CN" 一个变体，并删除 `EBLC`、`EBDT`、`EBSC`、
`BDF ` 四张点阵表，轮廓不变（24 像素渲染与原字体逐像素一致）。修改声明按 Arphic Public License 第 2(a) 条写在
字体 `name` 表的描述字段（name ID 10）。删除点阵表的原因：这些点阵来自 Firefly 补丁，来源存在争议（待核），
LiteOS 不分发它们；1x 改用来源清楚的文泉驿点阵。

选择依据（2026-10-24 对比渲染，脚本在工程内 `.local/spikes/classic-desktop/scripts/font-compare.py`）：
文泉驿点阵宋体最接近宋体 9 磅的点阵观感；文鼎 PL 明体 CN 轮廓比文鼎 PL 宋体 GB 更干净。

字体映射按 Windows 2000 GDI（实现见 `typeface::FontCatalog`），渲染按第 4 条以清晰为先：

1. 界面文字只使用主题 `[Fonts]` 中的逻辑字体（标题/小标题/菜单/状态栏/消息/图标字体，字体名、磅值、粗细），
   默认 Tahoma 9 磅（标题粗体），中文依次回退宋体与可分发替代。代码中不写字体文件或字号。
2. 字体目录扫描得到按 `name` 表家族名（中英文、TTC 各成员）索引的字体表；`fontmap.ini` 的 `[FontSubstitutes]`
   在字体名未安装时替换，`[SystemLink]` 是缺字回退链。原版 `simsun.ttc` 缺失时，宋体经链接由文泉驿点阵
   （12 像素）与文鼎明体（轮廓）提供；原版存在时它们只补缺字。
3. ppem = 磅值 × 96 × 倍率 / 72；字体有该 ppem 的点阵（BDF 像素尺寸相等，或 EBLC/EBDT 索引格式 1–5、图像格式 1/2/5）
   即用点阵，否则用轮廓。度量取首个可用字体的 OS/2 winAscent/winDescent（宋体 9 磅为 10+2）。
4. 字体渲染以高分屏与现代屏幕上的清晰为目标，不逐像素追随 Win2000（用户决定，2026-10-26）：主题 `FontSmoothing`
   默认 2（开启）。开启时，没有匹配点阵的字号一律输出未 hinting 的灰度轮廓，不受 gasp 表禁灰区间限制
   （宋体 gasp 规定 9–24 ppem 不灰度，scale 2 的 9 磅正是 24 ppem）；匹配的点阵（1x 宋体 12–18 像素）仍优先。
   Wine 注册表同步导出 `FontSmoothing`=2 与 `FontSmoothingType`=1（灰度）。关闭时按下条 hinting 与单色扫描转换。
   字体没有粗体成员时合成粗体：点阵右移 1 像素叠印，轮廓沿斜接方向加粗 `ppem / 24` 像素。
5. 平滑关闭时，带 `fpgm`/`prep`/`cvt` 的 TrueType 字体按 TrueType 指令集执行 hinting（`typeface::hinting`，语义对齐 FreeType v35、
   GETINFO 报告 35）：每字体执行一次 fpgm，每 ppem 执行 prep，每字形加四个幻影点执行字形指令，复合字形逐组件 hinting；
   距离与插值使用 16.16 定点和字体单位坐标。单色输出用 TrueType 扫描转换：像素中心规则 + 字体 SCANCTRL/SCANTYPE 指定的
   丢点控制（左/下侧补点、两侧已亮不补、1/5 排除残桩）。程序出错时退回未 hinting 的轮廓。
   对照（2026-10-26，宋体 12 像素与 Tahoma 8/10 磅可见 ASCII）：多数字形与 FreeType v35 单色逐像素一致，其余差 1–2 像素。
6. 字体必须作为独立数据文件随系统分发并在运行时加载，不得用 `include_bytes!` 等方式编入程序二进制：
   GPL 字体嵌入例外只覆盖"文档"，不明确覆盖程序。字体源码（BDF、SFD）与许可证随字体文件一起分发。

## 声音（`assets/classic/sounds-src/`）

仓库**不提供**原创声音：Codex 只生成图像，自制声音没有来源。没有本地原版素材时系统静音。声音是任意 PCM WAV，
使用方负责重采样（原版是 22050 Hz、16 位、立体声），不要求 48 kHz。

事件与文件的对应取自安装盘 `HIVEDEF.INF` 的默认 AppEvents：

| 名称 | 事件 |
|---|---|
| `logon` | 登录完成（`SystemStart`） |
| `logoff` | 注销 / 关机 |
| `critical`、`exclamation`、`asterisk`、`question` | 四种消息框；原版四者都是同一个 `chord.wav` |
