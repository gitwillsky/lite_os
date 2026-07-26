# ADR: 首次有声播放需要应用内用户激活

- 状态：已接受
- 日期：2026-07-26

## 背景

桌面应用具备 Native 文件能力不等于它可以在后台或开机时任意发声。完全禁止连续自动播放又会破坏
音乐播放器 playlist。播放授权属于 user agent policy，不能由系统 mixer 猜测。

## 决策

- 一个 app process 的首次 audible `HTMLMediaElement.play()` 必须源自该 app 收到的真实 pointer/key activation。
- 未授权的有声 `play()` Promise 以 `NotAllowedError` 拒绝，不创建系统逻辑 stream。
- muted media 可以启动；切换为 audible 时仍检查授权。
- 首次授权后形成 app-process-lifetime playback grant，允许 playlist 连续播放；app 退出即撤销。
- 系统通知音未来使用独立 Native notification capability，不能复用 media element 绕过该 policy。

## 结果

- LiteUI input/media owner 持有 playback grant；compositor 和系统音频服务不复制 user-activation 状态。
- synthetic event、timer、Promise job 和 background IPC 不能制造 activation。
- grant 缺失、元素卸载、app exit 和 desktop epoch reset 都有明确拒绝或回收结果。
