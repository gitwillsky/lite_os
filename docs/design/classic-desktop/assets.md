# 素材清单

仓库内位图素材由 Codex 按本文件的要求生成（流程见 `.claude/skills/generate-assets-with-codex`），源 PNG 放在
`assets/classic/<类别>-src/`，再由各自的打包步骤转成消费格式。文件名是契约：打包脚本与代码按名字取图。

## 本地原版素材

用户已选择保留 LiteOS 界面文案、优先使用自己的原版图像、声音和字体。提取 owner 是
`scripts/extract_windows2000_assets.py`，输出仅位于 Git 忽略的 `target/classic-desktop/windows2000/assets/`。
仓库中的原创素材和替代字体保留。当前产物是经过校验的素材目录，**尚未接入产品 rootfs**；
后续经典桌面在唯一素材打包入口选择来源，不引入第二套渲染实现。操作见
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
| `programs` / `documents` / `settings` | `shell32.dll:20` / `21` / `22` |
| `search` / `help` / `run` | `shell32.dll:23` / `24` / `25` |
| `shutdown` / `logoff` / `lock` / `key` | `shell32.dll:28` / `45` / `48` / `194` |
| `command-prompt` / `task-manager` / `media-player` | `cmd.exe:IDI_APPICON` / `taskmgr.exe:107` / `mplayer2.exe:110` |
| `show-desktop` / `display-properties` / `date-time` | `explorer.exe:103` / `desk.cpl:40` / `timedate.cpl:200` |
| `sound-volume` / `users` | `sndvol32.exe:304` / `netplwiz.dll:102` |
| `tray-volume` / `tray-volume-muted` | `stobject.dll:230` / `231`（托盘只有 16×16 原生图） |
| `msg-error` / `msg-warning` / `msg-info` / `msg-question` | `user32.dll:103` / `101` / `104` / `102` |
| `ctrl-alt-del` | `msgina.dll:109` 图标（键盘）；按原生尺寸保留，不冒充 48×48 位图 |
| `boot-scene` | `ntoskrnl.exe:1`，640×480 **完整启动画面**，不是下方原创的中央 `boot-logo` |
| `logon-banner` / `start-banner` | `msgina.dll:101`（413×72）/ `explorer.exe:157`（21×233） |

`liteos-logo` 沿用原创品牌图标。原版位图中烘焙的品牌文字不修改；动态文案始终为 LiteOS。

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
4. 倍率：每个素材都有 1x 与 2x 两套（HiDPI 规则见 [appearance.md](appearance.md)）。2x 是在 2 倍像素网格上重新
   绘制的版本，而不是 1x 的放大；两者的构图、配色与轮廓位置必须一致。
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

每个光标两张：`<名称>-32.png`（1x）与 `<名称>-64.png`（2x，硬件光标上限 64 × 64）。白色填充、1 像素黑色描边，
不自带阴影（光标阴影由 compositor 绘制，见 [interaction.md](interaction.md) E4）。热点以 1x 坐标给出，2x 乘以 2。

| 名称 | 语义 | 热点（1x） |
|---|---|---|
| `arrow` | 标准左上箭头 | (0, 0) |
| `help` | 箭头右侧带问号 | (0, 0) |
| `text` | I 形文本光标 | (8, 16)，待按图形居中修正 |
| `precision` | 细十字准星 | 图形中心 |
| `unavailable` | 圆圈加斜杠 | 图形中心 |
| `resize-ns` | 上下双向箭头 | 图形中心 |
| `resize-ew` | 左右双向箭头 | 图形中心 |
| `resize-nwse` | 左上—右下双向箭头 | 图形中心 |
| `resize-nesw` | 右上—左下双向箭头 | 图形中心 |
| `move` | 四向箭头 | 图形中心 |
| `link` | 伸出食指的手 | (6, 0)，食指指尖 |

动画光标（每帧一张，帧率 10 fps，循环）：

| 名称 | 帧 | 语义 | 热点 |
|---|---|---|---|
| `busy-<n>` | 0–7 | 沙漏：沙子自上而下流动并在最后一帧翻转 | 图形中心 |
| `working-<n>` | 0–7 | 标准箭头右下方附一个小沙漏，沙漏动画同 `busy` | (0, 0) |

## 位图（`assets/classic/bitmaps-src/`）

每张位图两个文件：`<名称>-1x.png` 与 `<名称>-2x.png`。

| 名称 | 1x 尺寸 | 2x 尺寸 | 语义 |
|---|---|---|---|
| `boot-logo` | 400 × 180 | 800 × 360 | 启动画面中央：LiteOS logo 在上，下方"LiteOS 2000 Professional"（"2000"较大、"Professional"较小）与一行小字"基于 LiteOS 技术"；白色背景（不透明） |
| `logon-banner` | 400 × 72 | 800 × 144 | 登录类对话框顶部横幅：左侧 logo，右侧"LiteOS 2000 Professional"，背景为浅灰到白的水平过渡，底部 1 像素深色线；不透明 |
| `start-banner` | 21 × 240 | 42 × 480 | 开始菜单左侧竖条：深海军蓝到黑的竖向渐变背景，文字自下而上竖排"LiteOS 2000 Professional"，"LiteOS"白色粗体、其余为浅灰；不透明。菜单高于素材时，素材底对齐，上方用素材顶部颜色填充 |
| `ctrl-alt-del` | 48 × 48 | 96 × 96 | "欢迎使用"对话框左侧：三个键帽（Ctrl、Alt、Del）的示意；透明背景 |

## 字体（`assets/fonts/`）

| 用途 | 倍率 | 文件 | 许可证文件 | 置信度 |
|---|---|---|---|---|
| 中文界面字体（宋体替代） | 1x | `wenquanyi_9pt.bdf`：文泉驿点阵宋体 12 像素点阵，30503 字 | `WenQuanYi-LICENSE`（GPL-2.0 + 字体嵌入例外） | 确定 |
| 中文界面字体（宋体替代） | 2x 及以上 | `ar-pl-uming-cn.ttf`：文鼎 PL 明体 CN 轮廓，24232 个码位 | `ARPHICPL.TXT`（Arphic Public License，原样保留） | 确定 |
| 西文界面字体（Tahoma 替代） | 全部 | `tahoma.ttf`、`tahomabd.ttf`（Wine 11.16 由 `tahoma.sfd`、`tahomabd.sfd` 构建） | `Wine-Tahoma-LICENSE`（LGPL-2.1+） | 确定 |
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

选择依据（2026-10-24 对比渲染，脚本在仓库外 `~/liteos-spike/scripts/font-compare.py`）：

1. 1x：文泉驿点阵宋体最接近宋体 9 磅的点阵观感；文鼎 PL 宋体 GB（`gbsn00lp`）在 12 像素下笔画粗细不均、字形歪斜，淘汰。
2. 2x：文鼎 PL 明体 CN 轮廓比文鼎 PL 宋体 GB 更干净。
3. 中文字体自带的拉丁字母明显不如 Tahoma，界面中的拉丁字母、数字与符号一律由 Tahoma 渲染。

渲染规则：

1. 1x 关闭抗锯齿，只输出 0/255 覆盖率；2x 及以上开启灰度抗锯齿。
2. 中文字体没有粗体：粗体（标题栏、"开始"按钮）用叠印合成，即同一字形横向偏移 1 个逻辑像素（scale N 下为 N 个
   物理像素）再画一次。拉丁字母的粗体直接用 `tahomabd.ttf`。
3. 字体必须作为独立数据文件随系统分发并在运行时加载，不得用 `include_bytes!` 等方式编入程序二进制：
   GPL 字体嵌入例外只覆盖"文档"，不明确覆盖程序。字体源码（BDF、SFD）与许可证随字体文件一起分发。

## 声音（`assets/classic/sounds-src/`）

48 kHz、16 位、立体声 WAV，原创，不使用 Windows 声音文件。Codex 只负责图像，声音来源**待定**（候选：
用合成器制作短促的和弦与提示音）。

| 名称 | 时长 | 语义 |
|---|---|---|
| `logon` | 约 4 秒 | 登录完成：温暖、上扬的合成器和弦 |
| `logoff` | 约 3 秒 | 注销 / 关机：与 `logon` 呼应的下行和弦 |
| `critical` | < 1 秒 | 错误：低沉的短促提示 |
| `exclamation` | < 1 秒 | 警告：中音提示 |
| `asterisk` | < 1 秒 | 信息：清脆提示 |
| `question` | < 1 秒 | 问题：同 `asterisk` 音色、略高 |
