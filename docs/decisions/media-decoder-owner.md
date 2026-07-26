# ADR: LiteUI audio worker 独占媒体解码

- 状态：已接受
- 日期：2026-07-26

## 背景

压缩媒体需要读取、demux、decode、seek 和预取。若系统音频服务同时拥有这些状态，
`HTMLMediaElement` 就必须镜像 timeline/network/ready/error 状态，并把 filesystem-backed `File`
跨进程转移给中心服务。若在 QuickJS/UI thread 同步解码，则一次慢 I/O 或 codec frame 会阻塞输入与渲染。

## 决策

每个 LiteUI 进程建立一个 audio worker，独占该进程全部 media element 的：

- `File`/Blob source 与按需读取；
- container/codec、decode cursor、seek 和 decoded PCM queue；
- 向系统音频服务建立、填充和销毁逻辑 stream；
- 把设备消费进度投影为 media element time/ended/error 事件。

QuickJS/UI thread 只发送异步命令并接收有界事件，不执行文件 I/O、decode、resample 或 IPC backpressure
等待。系统音频服务只接收统一 PCM、应用 gain 并混音，不解析媒体文件。

## 结果

- `HTMLMediaElement`、文件和 decoder 生命周期留在同一 app process；进程退出沿一个 cleanup path 回收。
- 一个进程只建立一个 audio worker，不能按 element 创建无界线程。
- future Web Audio 可以复用同一 worker 与系统 PCM stream seam，不建立第二个 mixer。
- codec error 只终止对应 media element；audio worker fatal failure 只终止所属 app，不影响全局 mixer。
