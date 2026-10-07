use crate::drivers::DisplayMode;

/// completion-confirmed hardware scanout 的 canonical 状态。
///
/// 该状态独立于 2D residency cache：VirGL framebuffer 可以在 resident slot
/// 全部退休后继续拥有 scanout。close 若从 residency 推导 active mode，会错误报告
/// 无 active resource，无法提交必须的 resource-id-zero disable command。
pub(super) struct ScanoutState {
    mode: Option<DisplayMode>,
}

impl ScanoutState {
    /// 创建尚无 hardware scanout binding 的状态。
    ///
    /// # Returns
    ///
    /// disabled scanout state。
    pub(super) const fn disabled() -> Self {
        Self { mode: None }
    }

    /// 发布已完成 2D 或 VirGL presentation 的 active mode。
    ///
    /// # Parameters
    ///
    /// - `mode`: completion-confirmed CRTC mode。
    pub(super) fn presented(&mut self, mode: DisplayMode) {
        self.mode = Some(mode);
    }

    /// 返回编码 scanout disable 所需的 exact active mode。
    ///
    /// # Returns
    ///
    /// active 时返回 mode，disabled 时返回 None。
    pub(super) const fn mode(&self) -> Option<DisplayMode> {
        self.mode
    }

    /// 完成 resource-id-zero disable transition。
    pub(super) fn complete_disable(&mut self) {
        self.mode = None;
    }
}

#[cfg(test)]
mod tests {
    use super::ScanoutState;
    use crate::drivers::DisplayMode;

    const MODE: DisplayMode = DisplayMode {
        width: 3008,
        height: 1692,
        pitch: 12_032,
    };

    #[test]
    fn virgl_presentation_remains_disableable_without_resident_state() {
        let mut scanout = ScanoutState::disabled();
        scanout.presented(MODE);
        assert_eq!(scanout.mode(), Some(MODE));

        scanout.complete_disable();
        assert_eq!(scanout.mode(), None);
    }
}
