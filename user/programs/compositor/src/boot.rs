//! 启动画面：复现内核 `boot_screen` 的最后一帧，使接管 scanout 时画面不跳动。

use boot_scene::KERNEL_PROGRESS_PERMILLE;
use linux_uapi::drm::{DumbBuffer, VirglResource};

mod paint;
pub(crate) use paint::{BootSurface, paint};

impl BootSurface for DumbBuffer {
    fn width(&self) -> usize {
        self.width()
    }
    fn height(&self) -> usize {
        self.height()
    }
    fn row_mut(&mut self, row: usize) -> &mut [u32] {
        self.row_mut(row)
    }
}
impl BootSurface for VirglResource {
    fn width(&self) -> usize {
        self.width_usize()
    }
    fn height(&self) -> usize {
        self.height_usize()
    }
    fn row_mut(&mut self, row: usize) -> &mut [u32] {
        self.row_mut(row)
    }
}

/// 内核交接时的进度。
pub const HANDOFF_PERMILLE: u16 = KERNEL_PROGRESS_PERMILLE;

/// 进度条每秒最多追赶多少千分点。目标由真实的启动里程碑给出，显示值只负责平滑地追上它，
/// 所以条的位置永远不超过已经真正完成的工作，又不会在里程碑之间一跳一跳地僵住。
const CATCH_UP_PERMILLE_PER_SECOND: f32 = 450.0;

/// 启动进度：显示值平滑追赶目标，目标只升不降。
pub struct Progress {
    shown: f32,
    target: u16,
}

impl Progress {
    pub fn new(start: u16) -> Self {
        Self {
            shown: f32::from(start),
            target: start,
        }
    }

    /// 提高目标（真实里程碑达成）；比当前目标低的值被忽略。
    ///
    /// # Returns
    ///
    /// 目标是否真的提高了。
    pub fn raise_target(&mut self, target: u16) -> bool {
        let raised = target.min(1000) > self.target;
        self.target = self.target.max(target.min(1000));
        raised
    }

    /// 经过 `seconds` 后推进显示值。
    pub fn advance(&mut self, seconds: f32) {
        self.shown =
            (self.shown + CATCH_UP_PERMILLE_PER_SECOND * seconds).min(f32::from(self.target));
    }

    pub fn permille(&self) -> u16 {
        self.shown as u16
    }

    /// 显示值还没追上目标，需要继续计时重绘。
    pub fn animating(&self) -> bool {
        self.permille() < self.target
    }

    pub fn complete(&self) -> bool {
        self.permille() >= 1000
    }
}

#[cfg(test)]
mod tests;
