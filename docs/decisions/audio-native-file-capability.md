# ADR: Web 媒体播放组合 Native 文件能力

- 状态：已接受
- 日期：2026-07-26

## 背景

音乐播放器需要访问桌面文件系统。LiteOS 的目标不是把所有桌面应用限制在浏览器网页的文件选择
沙箱内；现有应用已经通过 `lite:fs` 使用 Native 文件能力。同时，音频播放控制和状态仍应遵循 Web
标准，不能因来源是本地文件而分裂为私有播放器。

## 决策

应用可以用 Native 文件能力发现、选择和打开桌面文件，再把得到的媒体来源交给标准
`<audio>` / `HTMLMediaElement`。首个里程碑不要求通过 `<input type="file">` 才能访问本地音频，
也不以 Web 文件沙箱作为桌面应用的强制权限边界。

Native 文件能力只负责文件身份、访问授权和打开。`lite:fs.open(path)` 返回标准、filesystem-backed
`File`；应用通过 `URL.createObjectURL(file)` 生成 `blob:` URL 并交给 media element。文件内容按需读取，
不得整体复制进 QuickJS heap。解码、播放控制、媒体状态与事件仍由 media element 和系统音频链路唯一拥有。

## 结果

- 音乐播放器可以浏览 Native 文件系统，同时复用标准媒体播放组件。
- 不能新增按路径执行播放的 `lite:audio` 接口，否则会复制 media element 状态与 cleanup。
- `URL.revokeObjectURL()` 只阻止该 opaque URL 的未来解析；已经加载的 media 持有独立文件引用，
  直到 `src` 替换、空 `src`/`load()`、元素卸载或进程退出才沿唯一 close owner 释放。
- media element 不接受拥有 ambient filesystem authority 的绝对 `file:` 路径。
