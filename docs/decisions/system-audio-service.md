# ADR: 系统音频服务独占输出并混合逻辑 stream

- 状态：已接受
- 日期：2026-07-26

## 背景

桌面上可能同时存在音乐播放器、通知和其他媒体元素。让每个 LiteUI 进程直接打开 PCM 设备会造成
设备争用，并在进程间复制重采样、音量、underrun 和设备恢复状态。

## 决策

建立唯一系统音频服务，独占物理 PCM output 并混合多个逻辑 playback stream。每个
`HTMLMediaElement` 拥有自己的媒体状态和一条逻辑 stream；系统服务拥有设备时钟、mix buffer、
per-stream gain 应用、格式归一化、提交进度和 underrun 恢复。

该服务由 init 在开机时启动并监督，不创建窗口，也不进入 desktop 应用 registry。空闲时必须阻塞在
IPC/PCM readiness，不使用 timer 轮询或产生周期唤醒。LiteUI、compositor 和任一应用都不得承担
daemon 拉起或重启 owner。

服务固定两个线程：

- control thread 独占 AF_UNIX、handshake、quota、memfd/SCM_RIGHTS、stream lifecycle 与设置持久化；
- real-time mixer thread 独占 ALSA PCM、256-frame mix/limiter、device submission 与 consumption progress。

control→mixer 与 mixer→control 各使用一个预分配、有界 SPSC queue。mixer steady path 禁止 lock、
allocation、filesystem I/O、socket framing 或等待 control；queue pressure 必须形成 control-side
backpressure/typed error。进程退出先停止并释放 device，再撤销全部 stream/ring。

应用不直接访问系统服务协议；LiteUI media element adapter 代表应用建立和销毁 stream。Native
音频消费者若未来接入，也必须使用受支持的标准 native API，不能绕过系统 mixer 抢占设备。

## 结果

- 多个应用和多个 media element 可以同时播放。
- 系统静音和 master volume 只在系统服务中应用一次；media element 的 volume/muted 仍属于元素状态。
- client disconnect、元素卸载或 app 退出必须 exactly-once 回收 stream 和未消费 buffer。
- 首个 `play()` 只连接既有服务，不并发 spawn daemon；服务异常退出只由 init 重启。
- 系统服务崩溃不得拖垮 compositor 或 app：旧 stream 全部失效并静默停止，pending `play()` 以
  `AbortError` 拒绝，元素保留最后确认消费的 `currentTime` 并进入 paused/error 状态。
- init 重启服务后不得自动恢复声音；应用再次显式调用 `play()` 时才从保留时间新建 decoder/stream，
  app-process-lifetime playback grant 保持有效。
