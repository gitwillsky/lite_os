# 经典桌面设计规格（简体中文 Windows 2000 风格）

本目录是 LiteOS 经典桌面**外观与交互**的唯一验收标准。实现（`classic-theme`、`classic-ui`、`compositor`、
`logon`、`desktop` 及各应用）只引用这里的数值与行为，不另行发明；发现不一致时先改规格再改实现。

| 文档 | 内容 |
|---|---|
| [appearance.md](appearance.md) | 系统颜色、尺寸度量、字体、立体边框绘制规则、HiDPI |
| [boot-logon.md](boot-logon.md) | 开机、登录、注销、关机的画面序列与文案 |
| [shell.md](shell.md) | 桌面、任务栏、开始菜单、托盘与系统对话框 |
| [explorer.md](explorer.md) | 资源管理器、我的电脑、控制面板的布局、交互与范围缩减 |
| [interaction.md](interaction.md) | 逐条验收的交互清单 |
| [assets.md](assets.md) | 素材清单（文件名、1x/2x 尺寸、语义）与生成规则 |

## 资料来源

仓库保存原创素材与可分发的替代字体；用户提供的原版素材仅提取到 Git 忽略的 `target/`，
不提交到仓库。规格依据以下资料整理：

1. Microsoft Learn 的 Win32 文档：`GetSysColor`、`GetSystemMetrics`、`SystemParametersInfo`、`DrawEdge`、
   `DrawFrameControl`、`NONCLIENTMETRICS` 等 API 的语义与默认值说明。
2. Wine `user32`/`comctl32`（LGPL）作为行为参照实现：只对照语义与数值，不复制代码。
   阶段 0 已记录的结果用于 Win32 程序、协议与 ABI 验证。
3. 用户提供的 Windows 2000 简中 SP4 安装介质与静态资源。
   素材来源与映射见 [assets.md](assets.md)，操作见[构建与验证](../../development/build-and-verify.md#经典桌面本地素材)。

## 置信度

每个数值或行为标注一种置信度：

- **确定**：有 Win32 文档、原版资源数据或已记录的行为测量支持。
- **待核**：来自截图描述或印象。实现时可以先用，后续按公开规范与静态资源核对；
  未核实的数值不提升为"确定"。不再依赖 Windows 2000 参照机。

## 品牌替换

外观与交互还原 Windows 2000，品牌是 LiteOS 自己的：

| Windows 2000 原文 | LiteOS |
|---|---|
| Microsoft Windows 2000 Professional | LiteOS 2000 Professional |
| Built on NT Technology | 基于 LiteOS 技术 |
| Windows 旗帜 logo | LiteOS 自绘 logo（见 [assets.md](assets.md)） |
| 版权所有 © 1985-1999 Microsoft Corp. | 版权所有 © LiteOS 贡献者 |
| 文案中的"Windows"（如"关闭 Windows"、"登录到 Windows"、"Windows 安全"） | "LiteOS"（"关闭 LiteOS"、"登录到 LiteOS"、"LiteOS 安全"） |

通用交互文案（"开始"、"我的电脑"、"回收站"、"确定"、"取消"等）按原样保留。"Windows"是微软商标，
可交互界面文案继续使用 LiteOS；文案的结构、标点与快捷键字母保持不变。
本地原版位图内已经烘焙的 Windows 字样保留原始像素，不作为界面文案表的覆盖入口。
