# 外观规格

本文件的数值由 `classic-theme` 独占定义；`classic-ui`、`compositor`（启动画面、Alt+Tab）和 Wine 前缀注册表
（`Control Panel\Colors`、`Control Panel\Desktop\WindowMetrics`）都从它生成，不各自维护副本。
所有尺寸都是**逻辑像素**（96 DPI 基准），HiDPI 规则见文末。置信度定义见 [README.md](README.md)。

## 系统颜色（"Windows 标准"配色方案）

下表颜色已与用户提供安装介质的 `I386/HIVEDEF.INF` 中 `Control Panel\Colors` 默认值核对。

| 索引 | 名称 | 值 | 用途 | 置信度 |
|---|---|---|---|---|
| 0 | `COLOR_SCROLLBAR` | `#D4D0C8` | 滚动条轨道 | 确定 |
| 1 | `COLOR_BACKGROUND` | `#3A6EA5` | 桌面 | 确定 |
| 2 | `COLOR_ACTIVECAPTION` | `#0A246A` | 活动标题栏渐变左端 | 确定 |
| 3 | `COLOR_INACTIVECAPTION` | `#808080` | 非活动标题栏渐变左端 | 确定 |
| 4 | `COLOR_MENU` | `#D4D0C8` | 菜单背景 | 确定 |
| 5 | `COLOR_WINDOW` | `#FFFFFF` | 窗口客户区、编辑框 | 确定 |
| 6 | `COLOR_WINDOWFRAME` | `#000000` | 窗口外框、默认按钮框 | 确定 |
| 7 | `COLOR_MENUTEXT` | `#000000` | 菜单文字 | 确定 |
| 8 | `COLOR_WINDOWTEXT` | `#000000` | 客户区文字 | 确定 |
| 9 | `COLOR_CAPTIONTEXT` | `#FFFFFF` | 活动标题文字 | 确定 |
| 10 | `COLOR_ACTIVEBORDER` | `#D4D0C8` | 活动窗口缩放边框 | 确定 |
| 11 | `COLOR_INACTIVEBORDER` | `#D4D0C8` | 非活动窗口缩放边框 | 确定 |
| 12 | `COLOR_APPWORKSPACE` | `#808080` | MDI 工作区 | 确定 |
| 13 | `COLOR_HIGHLIGHT` | `#0A246A` | 选中项背景 | 确定 |
| 14 | `COLOR_HIGHLIGHTTEXT` | `#FFFFFF` | 选中项文字 | 确定 |
| 15 | `COLOR_BTNFACE`（`3DFACE`） | `#D4D0C8` | 按钮、对话框、任务栏面 | 确定 |
| 16 | `COLOR_BTNSHADOW`（`3DSHADOW`） | `#808080` | 立体边框内暗边 | 确定 |
| 17 | `COLOR_GRAYTEXT` | `#808080` | 禁用文字 | 确定 |
| 18 | `COLOR_BTNTEXT` | `#000000` | 按钮文字 | 确定 |
| 19 | `COLOR_INACTIVECAPTIONTEXT` | `#D4D0C8` | 非活动标题文字 | 确定 |
| 20 | `COLOR_BTNHIGHLIGHT`（`3DHILIGHT`） | `#FFFFFF` | 立体边框亮边 | 确定 |
| 21 | `COLOR_3DDKSHADOW` | `#404040` | 立体边框外暗边 | 确定 |
| 22 | `COLOR_3DLIGHT` | `#D4D0C8` | 立体边框内亮边 | 确定 |
| 23 | `COLOR_INFOTEXT` | `#000000` | 工具提示文字 | 确定 |
| 24 | `COLOR_INFOBK` | `#FFFFE1` | 工具提示背景 | 确定 |
| 26 | `COLOR_HOTLIGHT` | `#000080` | 超链接、热跟踪 | 确定 |
| 27 | `COLOR_GRADIENTACTIVECAPTION` | `#A6CAF0` | 活动标题栏渐变右端 | 确定 |
| 28 | `COLOR_GRADIENTINACTIVECAPTION` | `#C0C0C0` | 非活动标题栏渐变右端 | 确定 |

禁用文字采用"浮雕"画法：先在 (+1, +1) 用 `BTNHIGHLIGHT` 画一次，再在原位用 `BTNSHADOW` 画一次（确定）。

## 尺寸度量（96 DPI）

| 度量 | 值 | 说明 | 置信度 |
|---|---|---|---|
| `SM_CXBORDER` | 1 | 细边框 | 确定 |
| `SM_CXEDGE` | 2 | 立体边框厚度（两层各 1） | 确定 |
| `SM_CXFIXEDFRAME` | 3 | 对话框（不可缩放）边框 | 确定 |
| `SM_CXSIZEFRAME` | 4 | 可缩放窗口边框（`BorderWidth`=1 + 3） | 确定 |
| `SM_CYCAPTION` | 19 | 标题栏高（`CaptionHeight`=18 + 1 分隔线） | 确定 |
| 标题栏按钮区 | 18 × 18 | `CaptionWidth`×`CaptionHeight`；可见按钮为 16 × 14 | 确定 |
| 最大化与关闭按钮间距 | 2 | 最小化与最大化按钮紧贴 | 确定 |
| `SM_CYSMCAPTION` | 16 | 工具窗口标题栏（`SmCaptionHeight`=15 + 1） | 待核 |
| `SM_CYMENU` | 19 | 菜单栏高（`MenuHeight`=18 + 1） | 确定 |
| 菜单项高度 | 字体高 + 6，最少 17 | 下拉菜单项 | 待核 |
| `SM_CXVSCROLL` / `SM_CYHSCROLL` | 16 | 滚动条宽 | 确定 |
| `SM_CXICON` | 32 | 大图标 | 确定 |
| `SM_CXSMICON` | 16 | 小图标 | 确定 |
| `SM_CXICONSPACING` / `SM_CYICONSPACING` | 75 / 75 | 桌面图标网格 | 待核 |
| 任务栏高度 | 28 | 单行；顶边 2 像素立体亮边 | 待核 |
| 任务栏按钮 | 高 22，最大宽 160 | 按钮间距 2 | 待核 |
| "开始"按钮 | 高 22 | 宽度随文字；旗帜图标 16 | 待核 |

## 字体

简体中文版统一使用宋体（SimSun）9 磅，即 96 DPI 下 12 像素字形。下表是主题 `[Fonts]` 的默认值，可在主题中修改；
字体名到文件的映射与可分发替代见 [assets.md](assets.md)。
下表中的"宋体"指所选素材来源中的中文字体。

| 用途 | 字体 | 字号 | 粗细 | 置信度 |
|---|---|---|---|---|
| 活动/非活动标题 | 宋体 | 9 磅 | 粗体 | 待核 |
| 菜单 | 宋体 | 9 磅 | 常规 | 确定 |
| 消息框 | 宋体 | 9 磅 | 常规 | 确定 |
| 状态栏、工具提示 | 宋体 | 9 磅 | 常规 | 确定 |
| 桌面图标标题 | 宋体 | 9 磅 | 常规 | 确定 |
| 对话框（`MS Shell Dlg`） | 宋体 | 9 磅 | 常规 | 确定 |
| 开始菜单侧栏文字 | 自绘位图（见 [assets.md](assets.md)） | — | — | — |

中文粗体按字体类型合成（点阵叠印、轮廓加粗），规则与抗锯齿策略见 [assets.md](assets.md) 的"渲染规则"。文字一律在像素网格上对齐，不做次像素定位。

## 立体边框

边框由内外两层 1 像素线组成，下表的"左上/右下"指该层线的上边+左边、下边+右边。取色与
Win32 `DrawEdge` 一致（确定，可在 Wine `uitools.c` 中核对）。

| 样式 | 外层左上 | 外层右下 | 内层左上 | 内层右下 | 用于 |
|---|---|---|---|---|---|
| `EDGE_RAISED` | `3DLIGHT` | `3DDKSHADOW` | `BTNHIGHLIGHT` | `BTNSHADOW` | 窗口外框、按钮常态 |
| `EDGE_SUNKEN` | `BTNSHADOW` | `BTNHIGHLIGHT` | `3DDKSHADOW` | `3DLIGHT` | 编辑框、列表框、客户区凹边 |
| `EDGE_ETCHED` | `BTNSHADOW` | `BTNHIGHLIGHT` | `BTNHIGHLIGHT` | `BTNSHADOW` | 分组框、分隔线 |
| `BDR_RAISEDINNER` | — | — | `BTNHIGHLIGHT` | `BTNSHADOW` | 任务栏、状态栏分格（单层） |
| `BDR_SUNKENOUTER` | `BTNSHADOW` | `BTNHIGHLIGHT` | — | — | 托盘区、状态栏格（单层） |

按钮状态（`DrawFrameControl(DFC_BUTTON, DFCS_BUTTONPUSH)`，确定）：

1. **常态**：`EDGE_RAISED`，面色 `BTNFACE`。
2. **按下**：外层一圈 `WINDOWFRAME`（黑），内层一圈 `BTNSHADOW`，内容向右下偏移 1 像素。
3. **默认按钮**：在常态外再加一圈 1 像素 `WINDOWFRAME`，整体向内收缩 1 像素。
4. **焦点**：内容区内缩 3 像素处画 1 像素点状虚线框（每隔一个像素一点，异或绘制）。
5. **禁用**：文字用浮雕画法。

复选框与单选框为 13 × 13，`EDGE_SUNKEN` 加白色底（确定）。

## 窗口非客户区

1. 外框：可缩放窗口 `EDGE_RAISED` 外再加 2 像素 `ACTIVEBORDER` 面色，共 4 像素；对话框只有 `EDGE_RAISED` 加
   1 像素面色，共 3 像素。
2. 标题栏：高 18，左端 `ACTIVECAPTION` 向右端 `GRADIENTACTIVECAPTION` 水平线性渐变；非活动窗口用
   `INACTIVECAPTION` → `GRADIENTINACTIVECAPTION`。标题栏下方 1 像素 `BTNFACE` 分隔线。
3. 标题栏左侧：2 像素边距后放 16 × 16 小图标，再隔 2 像素放标题文字；文字单行，超长时以"..."截断。
4. 标题栏右侧按钮：最小化、最大化（或还原）、关闭，各 16 × 14，常态 `EDGE_RAISED`，图案用黑色
   字形；关闭按钮右边距 2 像素；不可用的按钮图案为浮雕禁用样式。
5. 菜单栏：标题栏下，高 18，背景 `MENU`；顶层菜单项悬停时显示 `BDR_RAISEDINNER` 单层凸起，
   按下时显示 `BDR_SUNKENOUTER` 单层凹陷（Windows 2000 的扁平菜单栏样式，待核）。

## 菜单

1. 下拉菜单外框 `EDGE_RAISED`，背景 `MENU`，无阴影（Windows 2000 没有菜单阴影，确定）。
2. 选中项背景 `HIGHLIGHT`、文字 `HIGHLIGHTTEXT`，整行高亮，左右各留 2 像素。
3. 勾选标记与单选圆点在左侧 16 像素宽的区域；子菜单箭头在右侧。
4. 分隔线为 `EDGE_ETCHED` 的水平线，上下各留 3 像素。
5. 快捷键文字右对齐，与菜单项文字之间至少 16 像素。

## 滚动条

1. 宽 16；两端箭头按钮 16 × 16，`EDGE_RAISED`，箭头为黑色实心三角。
2. 轨道为 `SCROLLBAR` 与 `BTNHIGHLIGHT` 的 1 像素棋盘格混色（确定：Win32 经典滚动条轨道是 50% 抖动图案）。
3. 滑块 `EDGE_RAISED`，最小长度 8（待核）。
4. 按住轨道时，被按下的那一侧轨道以反色棋盘格显示。

## 工具提示

`INFOBK` 背景、1 像素 `WINDOWFRAME` 外框，文字 `INFOTEXT`，四周留 2 像素；淡入显示（见 [interaction.md](interaction.md)）。

## HiDPI

1. compositor 按输出选择整数 scale：逻辑高度 = 物理高度 / scale，取使逻辑高度不低于 768 的最大整数
   （3008 × 1692 → scale 2，逻辑 1504 × 846；1920 × 1080 → scale 1）。分数 scale 不在本期范围。
2. 本文件所有逻辑像素在 scale N 下乘以 N：1 像素的立体线画成 N 像素宽，几何比例与 96 DPI 完全一致。
3. 文字不放大位图：逻辑字体按 磅值 × 96 × 倍率 / 72 的 ppem 直接选择点阵或灰度轮廓（规则见 [assets.md](assets.md)，以清晰为先）。
4. 图标、光标、位图按 [assets.md](assets.md) 选择对应倍率的素材；缺少对应倍率时用最近邻整数放大，
   禁止非整数缩放。
5. Wine 前缀的 `LogPixels` 设为 96 × N。
