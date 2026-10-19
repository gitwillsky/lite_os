#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeviceState {
    Setup,
    Configured,
    Prepared,
    Running,
    Stopped,
    Failed,
}

impl DeviceState {
    pub(super) fn after_configure(self) -> Option<Self> {
        (self == Self::Setup).then_some(Self::Configured)
    }

    pub(super) fn after_prepare(self) -> Option<Self> {
        matches!(self, Self::Configured | Self::Stopped).then_some(Self::Prepared)
    }

    pub(super) fn after_start(self) -> Option<Self> {
        matches!(self, Self::Prepared | Self::Stopped).then_some(Self::Running)
    }

    pub(super) fn after_stop(self) -> Option<Self> {
        (self == Self::Running).then_some(Self::Stopped)
    }

    pub(super) fn after_release(self) -> Option<Self> {
        matches!(self, Self::Configured | Self::Prepared | Self::Stopped).then_some(Self::Setup)
    }

    /// 进入 fail-stop；返回 true 表示 caller 是 reset/notification 的唯一发布者。
    pub(super) fn fail(&mut self) -> bool {
        if *self == Self::Failed {
            false
        } else {
            *self = Self::Failed;
            true
        }
    }
}

/// 以 completion head 唯一认领 outstanding slot；unknown/duplicate identity 都失败。
pub(super) fn unique_slot_for(
    completion_head: u16,
    slot_count: usize,
    mut head_at: impl FnMut(usize) -> Option<u16>,
) -> Option<usize> {
    let mut found = None;
    for index in 0..slot_count {
        if head_at(index) == Some(completion_head) {
            if found.is_some() {
                return None;
            }
            found = Some(index);
        }
    }
    found
}

/// 同步 control polling 是否消费了 device-wide interrupt edge。
///
/// VirtIO-MMIO 把所有 queue 合并到同一 interrupt-status bit。因此 control queue polling
/// 只要 ack 了 status 就必须发布 deferred work：同一 edge 可能同时覆盖 TX completions，
/// 丢弃它会让 full-buffer waiter 永久等待一个不会再出现的 IRQ。
pub(super) fn polled_control_ack_requires_deferred(interrupt_status: u32) -> bool {
    interrupt_status != 0
}
