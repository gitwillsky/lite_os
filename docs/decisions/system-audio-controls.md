# ADR: 系统音量由 mixer 独占并只向 desktop 开放控制

- 状态：已接受
- 日期：2026-07-26

## 背景

`HTMLMediaElement.volume/muted` 是元素局部状态，不能代表整个桌面的 master gain。让普通应用修改
master 会使应用彼此干扰；让 desktop 保存一份 volume 再同步到 mixer 会产生双 owner。

## 决策

- 系统音频服务唯一持有 master volume 与 mute，并只在最终 mix output 应用一次 master gain。
- 每个 media element 的 `volume/muted` 仍由 element/audio worker 投影为 per-stream gain。
- desktop-only Native 模块 `lite:audio-system` 订阅并修改 master 状态；普通 app session 必须被拒绝。
- React desktop 的任务栏扬声器图标显示当前 mute/volume，点击后打开系统音量控件。
- desktop 只保存用于绘制的最新只读 snapshot，不拥有 authoritative volume state。
- master volume/mute 跨重启持久化；首次启动默认 75%、unmuted。
- UI 连续修改只更新内存；最后一次变化 500 ms 后由服务以 temporary file→`fsync`→rename 原子提交，
  clean shutdown 刷新待写状态。损坏文件保留并诊断，服务使用安全默认值，不能静默覆盖损坏现场。
- `HTMLMediaElement.volume` 使用标准 `0..1` linear amplitude gain；master slider 使用
  `gain = (percent / 100)³` 的感知曲线，0% 精确 mute、100% 精确 0 dB。曲线只在设置变化时计算，
  mixer 热路径只读取缓存的 `f32` gain。

## 结果

- 应用不能修改其他应用或系统整体音量。
- desktop 重启后从系统服务重新取得状态，不把旧 UI snapshot 写回 mixer。
- mixer 对 master/per-stream gain 的应用顺序固定，不能在 worker、service 和 host backend 重复缩放。
- crash 最多丢失最后 500 ms 尚未提交的 UI 调整，不得损坏上一份完整状态。
