//! 工具提示：指针在同一目标上停留一小段时间后显示，按下按钮或离开目标即消失。

use std::time::{Duration, Instant};

/// 指针停留多久后显示（Windows 的默认初始延迟）。
pub const DELAY: Duration = Duration::from_millis(500);

/// 一个窗口内的工具提示状态；目标用调用者定义的序号表示。
#[derive(Debug, Default)]
pub struct Tooltip {
    target: Option<usize>,
    since: Option<Instant>,
    /// 点击后在指针离开当前目标之前不再显示；缺少它，点完按钮提示会立刻再冒出来盖住结果。
    dismissed: bool,
}

impl Tooltip {
    /// 指针现在位于 `target`（`None` 表示不在任何目标上）。换目标会重新计时。
    pub fn hover(&mut self, target: Option<usize>) {
        if self.target != target {
            self.target = target;
            self.since = target.map(|_| Instant::now());
            self.dismissed = false;
        }
    }

    /// 按下按钮：隐藏提示，直到指针移到别的目标。
    pub fn dismiss(&mut self) {
        self.dismissed = true;
    }

    /// 应当显示提示的目标；未到延迟、被点击隐藏或没有目标时为 `None`。
    pub fn visible(&self) -> Option<usize> {
        match (self.target, self.since) {
            (Some(target), Some(since)) if !self.dismissed && since.elapsed() >= DELAY => {
                Some(target)
            }
            _ => None,
        }
    }

    /// 距离提示出现还需要等待多久；没有等待中的提示时为 `None`。事件循环据此设置超时。
    pub fn pending(&self) -> Option<Duration> {
        let since = self.since?;
        if self.target.is_none() || self.dismissed {
            return None;
        }
        // 只采样一次时钟；两次 elapsed 之间越过 DELAY 会令原先的 Duration 减法 panic。
        DELAY
            .checked_sub(since.elapsed())
            .filter(|remaining| !remaining.is_zero())
    }
}
