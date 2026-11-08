# 经典桌面资源

经典界面素材的唯一输入位于受 Git 管理的 `assets/classic/`，界面位图、系统字体、声音与光标来自用户提供的
Windows 2000 Professional SP4 简中 ISO。ISO、原始 PE、全部导出资源与中间 PNG 留在被 Git 忽略的
`.local/inputs/windows2000/`；它们不参与普通构建，clone 后无需重新准备 ISO。

`scripts/extract_windows2000_assets.py` 拥有提取、转换和发布；`scripts/classic_assets.py` 校验并返回
受版本管理的产品清单，并由这些唯一输入生成安装图集。不存在本地覆盖、原创默认素材或替代字体的选择分支。任何必需文件缺失、损坏或
未登记冗余文件都会使构建失败。

`manifest.json` 记录 ISO 名称、字节数、SHA256、语言、工具版本，以及逐产物字节数、SHA256、来源资源号和转换方式。
只发布当前消费者需要的原生尺寸；不提交 ICO/PNG/BMP 三套重复表示，不保留无调用者的素材，也不生成伪造 64px 原图。
图标清单与尺寸由导入器的 `ICONS` / `ICON_SIZES` 拥有；新增消费者时在此入口增加明确的 ISO 资源映射。

## 导入与校验

只有更新素材时需要宿主离线工具和本地 ISO。

```bash
source ~/.zshrc
brew install sevenzip cabextract icoutils
python3 -m venv .local/tools/classic-assets
.local/tools/classic-assets/bin/pip install Pillow==13.0.0
.local/tools/classic-assets/bin/python scripts/extract_windows2000_assets.py
python3 scripts/extract_windows2000_assets.py --check
```

ISO 默认位于 `.local/inputs/windows2000/Windows 2000 Professional Service Pack 4 CHS.iso`。
导入器只读取 ISO/PE 数据，将完整提取结果保留在 `.local/inputs/windows2000/assets/`，
只转换并发布产品依赖到 `assets/classic/`，同步删除旧素材。提取配方改变时使用新的 `--output` 目录。
提交生成的产品文件与 manifest；不提交 ISO、DLL、中间 PNG 或无引用的资源。
`--check` 无需 ISO、Pillow 和外部提取工具。普通构建与开发镜像同步使用同一份清单，资源内容计入构建及同步指纹。

## 安装图集

LUIA0001 是只读产品数据格式，不是应用 ABI。所有整数为小端，应用仍通过标准 UI 资源接口取图。
32 字节头依次为 magic[8]、记录数 u32、记录长 u32（128）、文件长 u64、保留零 u64。
每记录按 ASCII 文件名排序：96 字节 NUL 补齐名称，width/height 各 u32，像素/下标偏移各 u64，保留零 u64。
索引后的数据区向上对齐到 4096；像素为 top-down 预乘 ARGB u32，下标为原始 u8（无下标时偏移零），各段向上对齐到 4 字节。

classic-ui 的 Bitmap 保活只读 MAP_PRIVATE 视图；每进程 Weak 目录只合并活视图，不强行保留已经无消费者的映射。
尺寸、排序、偏移、整像素对齐和文件内范围验证成功后才借出 u32 切片；绘制不持锁、不解码、不复制像素。
不同进程读同一文件的页缓存，只有触及页驻留；调色板重着色保留共享下标，只有新颜色像素私有。
图集更新必须用 rename 发布新 inode，不能原地写入/截断映射中的文件。当前安装/删除由离线 userland 同步完成，
全量迁移需同步库/原生程序及图集；现有 SSH 单文件部署不会自动删除旧进程仍需的资源。

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
| `recycle-bin-full` | `shell32.dll:33` |
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
| `media-transport-default` | `msdxm.ocx:5208` |
| `media-transport-disabled` | `msdxm.ocx:5209` |
| `media-volume-default` | `msdxm.ocx:5212` |
| `media-logo` | `msdxm.ocx:140` |
| `webview-left` | `webvw.dll` 类型 23 `WVLEFT.BMP` |
| `webview-line` | `webvw.dll` 类型 23 `WVLINE.GIF` |

动态界面文案保持 LiteOS；原版位图中烘焙的品牌文字与图像保留原始像素。
“我的文档”使用 `mydocs.dll:100`，开始菜单“文档”使用 `shell32.dll:21`，二者不得混用。

## 产品格式与引用

- `ui/`：标准 top-down BMP V4；图标保留原生尺寸和 alpha，同尺寸多色深变体取最高色深。
  普通图像为 32-bit BI_BITFIELDS；时区地图保留 8-bit BI_RGB 调色板下标，供标准 Image 控件更换调色板。
  工具栏条的洋红透明键在导入时转成 alpha 0。这些受版本管理的 BMP 是构建及原版画面比对的输入。
  `classic_assets::shell_atlas` 经现有 `pack_shell_assets` 转换器产生 `target/classic-ui/<fingerprint>/bitmaps.atlas`，
  普通构建只需 Python 标准库，不依赖 ISO/Pillow；指纹覆盖全部输入内容及转换器，复用既有锁和原子发布。
  产品安装只发布 `/usr/share/liteos/ui/bitmaps.atlas`，旧逐图 BMP 由离线同步清单退休。
  图标的同尺寸/较大/较小选择与缺尺寸最近邻缩放也在构建时完成；派生 64px 不作为 ISO 原生素材登记。
  运行库将旧资源路径解释为图集内的逻辑名，图集错误不回退运行时解码。用户/应用 PNG、BMP、JPEG 仍由通用解码器负责。
- `boot-scene.lbs`：`ntoskrnl.exe:1` 完整 640×480 启动画面，无损调色板 RLE，kernel 与 compositor 消费同一份文件。
- `cursors.lcs`：安装介质的 `*_r.cur` 默认方案；热点取 CUR 目录，2x 为整数最近邻放大。
  手形来自 `user32.dll:114`，保留其 CUR 热点 (10,10)，不混用原创光标。
  默认方案全部为静态帧；LCS1/硬件 ARGB 光标不能表达背景反色，导入遇到反色像素即失败，不静默改变掩码语义。
  其他方案和 ANI 没有消费者，留在 `.local/`，不进入产品资源。
- `sounds/`：默认声音方案实际引用的 `logon.wav`、`logoff.wav`、`chord.wav`、`ding.wav`（HIVEDEF.INF AppEvents）与
  `start.wav`（ieuinit.inf 的“启动导航”），保持 ISO 原始字节；`start.wav` 是 Microsoft ADPCM，由 `sound` 解码，帧数以 fact 块为准。
  `recycle.wav` 只属于 mmopt.inf 的 Utopia 方案，默认方案的“清空回收站”是 `ding.wav`，因此不导入。
- 消息框图标：`message-warning/question/error/information` 取 user32.dll 101/102/103/104（MB_ICON*），只发布 32 像素。
- `fonts/`：原始 `simsun.ttc`、`tahoma.ttf`、`tahomabd.ttf`、`marlett.ttf` 与唯一的 `fontmap.ini` 配置。
  SimSun/NSimSun 两个 face 和 12/13/14/15/16/18 ppem 内嵌点阵全部保留。Tahoma 中文经标准 FontLink 进入宋体；
  只安装 manifest 登记的原版字体。`fontmap.ini` 是 LiteOS 配置，不冒充 ISO 文件。
  标题按钮从 Marlett 的 Windows Symbol cmap 读取 U+F030/F031/F032/F072，按物理字号重新 hinting，
  不再手画最小化、最大化、还原和关闭的低分辨率点阵；普通文本不自动转换到符号私用区。

字体 ppem = 磅值 × 96 × scale / 72；匹配原生点阵时用点阵，否则渲染原版轮廓并执行 hinting。
主题的灰度平滑策略保持不变；9pt 在 scale=2 时为 24ppem，宋体没有这个字号的点阵，因此使用原版轮廓，
不能把字体来源恢复等同于所有倍率均为点阵字体。

`assets/fonts/` 中终端 atlas、JetBrains 源字体及许可证仍由终端与蓝屏生成器实际使用；它们不属于系统界面字体回退链。
主题颜色、窗口度量与字体角色属于 `theme/classic.theme`，不与 ISO 二进制素材混作同一所有权。
