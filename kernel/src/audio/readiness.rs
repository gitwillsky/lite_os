const POLLOUT: i16 = 0x004;

/// @description 把 PCM 当前可写 level 投影为 caller 请求的 Linux poll event。
///
/// @param events caller 的 logical poll mask。
/// @param writable ALSA state 与 physical queue 都允许再提交一个 period。
/// @return 可写时返回 caller 请求的 `POLLOUT`，否则返回零。
const fn project_playback_events(events: i16, writable: bool) -> i16 {
    if writable { events & POLLOUT } else { 0 }
}
