# ADR: 首个声音 Web API 使用 HTMLMediaElement

- 状态：已接受
- 日期：2026-07-26

## 背景

LiteUI 的目标是桌面环境实践 Web 标准。声音能力必须能够支撑音乐播放器等桌面应用，而不只是由
特定应用调用私有 native bridge 播放提示音。Web Audio API 的实时音频图、精确调度和合成语义明显
大于首个播放里程碑。

## 决策

首个公开声音接口实现标准 `<audio>` / `HTMLMediaElement` 播放语义，包括：

- 方法：`load()`、`play()`、`pause()`、`canPlayType()`；
- 可写属性：`src`、`currentTime`、`volume`、`muted`、`loop`、`preload`、`autoplay`、`controls`；
- 只读属性：`currentSrc`、`duration`、`paused`、`ended`、`seeking`、`buffered`、`seekable`、
  `readyState`、`networkState`、`error`；
- 事件：`loadstart`、`emptied`、`durationchange`、`loadedmetadata`、`loadeddata`、`canplay`、`play`、
  `playing`、`pause`、`waiting`、`seeking`、`seeked`、`timeupdate`、`ended`、`volumechange`、`abort`、`error`；
- React `ref` 返回可操作的 media element public instance。
- 标准 `controls` attribute：LiteUI UA 绘制 XP 主题的播放/暂停、进度、时间、音量与静音控件；
  无 `controls` 时元素不绘制内建 UI。

`timeupdate` steady cadence 最多每 250 ms 一次，并在 pause、seek 与 ended transition 补发精确状态。
ready/network/MediaError 常量和值必须与事件状态同步，不能只提供字段壳。

首期 `src` 只接受 app-relative build-time resource 与 `blob:` URL；blob backing 可以是 Native
`lite:fs.open()` 返回的 filesystem-backed `File`。不支持 `http:`/`https:`、`data:`、MediaSource 或
MediaStream。future network media 必须先建立统一 Fetch/CORS/cache/range owner，audio worker 不得私自
实现 downloader。

不提供 `lite:audio`、路径播放函数或其他私有应用 API。Web Audio API 不属于首个里程碑，未来必须
复用同一系统播放服务，不能建立第二条设备或混音路径。

首期固定 1× playback，不开放非 1.0 `playbackRate` 或 `preservesPitch`。正确的默认变速必须保持
音高；简单改变 sample consumption rate 会变调，不能作为标准实现。future time-stretch 必须复用
audio worker，并单独建立质量和 CPU gate。

设置 `currentTime` 必须发布 `seeking`，通过 PCM generation transaction 清除全部旧排队音频，decoder
seek 到不晚于目标的合法 frame 后 decode/discard 至精确目标 PCM frame，再恢复并发布 `seeked`。
失败进入 paused/error，不能继续播放旧位置或用近似 container offset 冒充成功。

## 结果

- 音乐播放器与其他应用只依赖 Web 标准，不感知 kernel PCM ABI 或系统服务协议。
- 未实现的 media 属性、codec 或事件必须保持接口不存在或显式失败，不能伪造成功。
- rate/track、encrypted media、Media Source、MediaStream 与 remote playback 首期保持不存在。
- author CSS 只控制 `<audio>` element box，不能穿透 UA controls；控件只操作同一 media element
  public instance，不能成为播放状态的第二 owner。
