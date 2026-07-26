/// @description 返回承载 PCM logical readiness edge 的 notification-pipe wait event。
///
/// Notification pipe 的物理 read side 只能产生 `POLLIN`；最终 PCM `POLLOUT` 必须由 caller
/// 醒来后重新查询 OFD level。若直接把 `POLLOUT` 注册到 read source，completion 虽会写入
/// token，但 wait registry 永远无法匹配该 edge。
const fn audio_notification_wait_event() -> i16 {
    POLLIN
}
