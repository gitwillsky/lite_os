# ADR: 首期交付只使用公开能力的 Music Player

- 状态：已接受
- 日期：2026-07-26

## 背景

host/unit test 可以证明局部 API，却不能证明普通桌面应用能在不接触私有音频协议的情况下完成真实音乐
播放。只提供特权 test app 还可能掩盖 File/Blob、media event 与 React public instance 的断层。

## 决策

首期 app registry 增加 production Music Player。它只能使用：

- `lite:fs` 浏览并打开 Native 文件；
- filesystem-backed `File` 与 `URL.createObjectURL()`；
- 标准 `<audio>` public instance、属性、方法与事件；
- 普通 React/CSS 与 design-system。

功能限定为目录/playlist、metadata、播放/暂停、上一首/下一首、seek、loop、element volume 与错误提示。
不包含 network library、跨曲 sample-accurate gapless、equalizer、format conversion 或 recording。

## 结果

- Music Player 不依赖 `audio-proto`、ALSA、VirtIO 或 system mixer interface；出现该依赖即架构失败。
- app close 必须 revoke 全部 blob URL、unmount media element 并释放 File/stream。
- 自动 runtime gate 使用同一 public app path，不安装 production 不存在的 privileged playback API。
