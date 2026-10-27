# 经典桌面资源

产品资源唯一位于受 Git 管理的 `assets/classic/`，全部图像、系统界面字体、声音与光标来自用户提供的
Windows 2000 Professional SP4 简中 ISO。ISO、原始 PE、全部导出资源与中间 PNG 留在被 Git 忽略的
`.local/inputs/windows2000/`；它们不参与普通构建，clone 后无需重新准备 ISO。

`scripts/extract_windows2000_assets.py` 拥有提取、转换和发布；`scripts/classic_assets.py` 只校验并返回
受版本管理的产品清单。不存在本地覆盖、原创默认素材或替代字体的选择分支。任何必需文件缺失、损坏或
未登记冗余文件都会使构建失败。导入操作见[构建与验证](../../development/build-and-verify.md#经典桌面素材导入)。

`manifest.json` 记录 ISO 名称、字节数、SHA256、语言、工具版本，以及逐产物字节数、SHA256、来源资源号和转换方式。
只发布当前消费者需要的原生尺寸；不提交 ICO/PNG/BMP 三套重复表示，不保留无调用者的素材，也不生成伪造 64px 原图。
图标清单与尺寸由导入器的 `ICONS` / `ICON_SIZES` 拥有；新增消费者时在此入口增加明确的 ISO 资源映射。

## ISO 身份与资源映射

输入为 402626560 字节，SHA256：
`a8e8d656bd147700c8c73e2ef0a47de7832b6c430d442f539f731db180ea8cb0`。
这是内容标识，不声称已认证发行方。PE 资源使用简中语言 2052；下面为资源名/号，不是零起始 icon index。

| 逻辑名 | ISO 来源与资源名/号 |
|---|---|
| `windows-logo` | `user32.dll:105` |
| `my-computer` | `shell32.dll:16` |
| `my-documents` | `mydocs.dll:100` |
| `recycle-bin-empty` | `shell32.dll:32` |
| `folder-closed` | `shell32.dll:4` |
| `file-generic` | `shell32.dll:1` |
| `file-text` | `shell32.dll:152` |
| `file-app` | `shell32.dll:3` |
| `drive-hdd` | `shell32.dll:9` |
| `drive-cd` | `shell32.dll:12` |
| `file-system` | `shell32.dll:153` |
| `control-panel` | `shell32.dll:137` |
| `programs` | `shell32.dll:20` |
| `documents` | `shell32.dll:21` |
| `settings` | `shell32.dll:22` |
| `help` | `shell32.dll:24` |
| `run` | `shell32.dll:25` |
| `shutdown` | `shell32.dll:28` |
| `logoff` | `shell32.dll:45` |
| `lock` | `shell32.dll:48` |
| `command-prompt` | `cmd.exe:IDI_APPICON` |
| `task-manager` | `taskmgr.exe:107` |
| `media-player` | `mplayer2.exe:110` |
| `show-desktop` | `explorer.exe:103` |
| `display-properties` | `desk.cpl:40` |
| `date-time` | `timedate.cpl:200` |
| `sound-volume` | `sndvol32.exe:304` |
| `users` | `netplwiz.dll:102` |
| `device-manager` | `devmgr.dll:201` |
| `network-connections` | `netshell.dll:10010` |
| `network-local` | `netshell.dll:122` |
| `network-disabled` | `netshell.dll:121` |
| `tray-network` | `netshell.dll:190` |
| `tray-network-disconnected` | `netshell.dll:195` |
| `network-disconnected` | `netshell.dll:123` |
| `network-places` | `shell32.dll:18` |
| `entire-network` | `shell32.dll:14` |
| `desktop` | `shell32.dll:35` |
| `notepad` | `notepad.exe:1` |
| `tray-volume` | `stobject.dll:230` |
| `tray-volume-muted` | `stobject.dll:231` |
| `ctrl-alt-del` | `msgina.dll:109` |
| `boot-scene` | `ntoskrnl.exe:1` |
| `logon-banner` | `msgina.dll:101` |
| `start-banner` | `explorer.exe:157` |
| `toolbar-default` | `browseui.dll:272` |
| `toolbar-hot` | `browseui.dll:273` |
| `toolbar-disabled` | `browseui.dll:276` |
| `ie-toolbar-default` | `browseui.dll:261` |
| `ie-toolbar-hot` | `browseui.dll:262` |
| `ie-toolbar-disabled` | `browseui.dll:265` |
| `go-default` | `browseui.dll:290` |
| `go-hot` | `browseui.dll:291` |
| `brand-22` | `browseui.dll:306` |
| `brand-26` | `browseui.dll:305` |
| `brand-38` | `browseui.dll:304` |
| `worldmap` | `timedate.cpl:50` |
| `webview-left` | `webvw.dll` 类型 23 `WVLEFT.BMP` |
| `webview-line` | `webvw.dll` 类型 23 `WVLINE.GIF` |

动态界面文案保持 LiteOS；原版位图中烘焙的品牌文字与图像保留原始像素。
“我的文档”使用 `mydocs.dll:100`，开始菜单“文档”使用 `shell32.dll:21`，二者不得混用。

## 产品格式与引用

- `ui/`：标准 top-down BMP V4；图标保留原生尺寸和 alpha，同尺寸多色深变体取最高色深。
  普通图像为 32-bit BI_BITFIELDS；时区地图保留 8-bit BI_RGB 调色板下标，供标准 Image 控件更换调色板。
  工具栏条的洋红透明键在导入时转成 alpha 0。Resources/AppIcons 在加载时按请求尺寸选择原图；缺失尺寸仅做最近邻缩放。
- `boot-scene.lbs`：`ntoskrnl.exe:1` 完整 640×480 启动画面，无损调色板 RLE，kernel 与 compositor 消费同一份文件。
- `cursors.lcs`：安装介质的 `*_r.cur` 默认方案；热点取 CUR 目录，2x 为整数最近邻放大。
  手形来自 `user32.dll:114`，保留其 CUR 热点 (10,10)，不混用原创光标。
  默认方案全部为静态帧；LCS1/硬件 ARGB 光标不能表达背景反色，导入遇到反色像素即失败，不静默改变掩码语义。
  其他方案和 ANI 没有消费者，留在 `.local/`，不进入产品资源。
- `sounds/`：`logon.wav`、`logoff.wav`、`chord.wav`、`recycle.wav`，保持 ISO 的原始 PCM 字节与参数。
- `fonts/`：原始 `simsun.ttc`、`tahoma.ttf`、`tahomabd.ttf`、`marlett.ttf` 与唯一的 `fontmap.ini` 配置。
  SimSun/NSimSun 两个 face 和 12/13/14/15/16/18 ppem 内嵌点阵全部保留。Tahoma 中文经标准 FontLink 进入宋体；
  不再安装文泉驿、UMing 或 Wine Tahoma。`fontmap.ini` 是 LiteOS 配置，不冒充 ISO 文件。
  标题按钮从 Marlett 的 Windows Symbol cmap 读取 U+F030/F031/F032/F072，按物理字号重新 hinting，
  不再手画最小化、最大化、还原和关闭的低分辨率点阵；普通文本不自动转换到符号私用区。

字体 ppem = 磅值 × 96 × scale / 72；匹配原生点阵时用点阵，否则渲染原版轮廓并执行 hinting。
主题的灰度平滑策略保持不变；9pt 在 scale=2 时为 24ppem，宋体没有这个字号的点阵，因此使用原版轮廓，
不能把字体来源恢复等同于所有倍率均为点阵字体。

`assets/fonts/` 中终端 atlas、JetBrains 源字体及许可证仍由终端与蓝屏生成器实际使用；它们不属于系统界面字体回退链。
主题颜色、窗口度量与字体角色属于 `theme/classic.theme`，不与 ISO 二进制素材混作同一所有权。
