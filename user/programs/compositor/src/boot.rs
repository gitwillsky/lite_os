//! 启动画面：复现内核 `boot_screen` 的最后一帧，使接管 scanout 时画面不跳动。

use boot_scene::{KERNEL_PROGRESS_PERMILLE, Layout, ParseError, Scene, WIDTH};
use linux_uapi::drm::DumbBuffer;

/// 启动画面绘制失败。
#[derive(Debug)]
pub enum BootError {
    Scene(ParseError),
    /// 屏幕小于 640 × 480，内核同样不显示启动画面。
    ScreenTooSmall,
}

impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Scene(error) => write!(f, "invalid boot scene: {error:?}"),
            Self::ScreenTooSmall => write!(f, "screen is smaller than the 640x480 boot scene"),
        }
    }
}

impl std::error::Error for BootError {}

/// 整屏清黑，绘制整数放大并居中的位图，再涂到 `permille` 进度。
///
/// # Parameters
///
/// - `bytes`: `LBS1` 文件内容。
/// - `permille`: 进度条填充（0..=1000）；接管内核画面时用 [`KERNEL_PROGRESS_PERMILLE`]。
///
/// # Errors
///
/// 位图格式非法或屏幕太小。
pub fn paint(target: &mut DumbBuffer, bytes: &[u8], permille: u16) -> Result<(), BootError> {
    let scene = Scene::parse(bytes).map_err(BootError::Scene)?;
    let layout = Layout::fit(target.width(), target.height()).ok_or(BootError::ScreenTooSmall)?;
    for row in 0..target.height() {
        target.row_mut(row).fill(0);
    }
    let mut indices = [0u8; WIDTH];
    let mut rows = scene.rows();
    let mut source_row = 0;
    while rows.next_row(&mut indices) {
        for repeat in 0..layout.scale {
            let line = target.row_mut(layout.y + source_row * layout.scale + repeat);
            let line = &mut line[layout.x..layout.x + WIDTH * layout.scale];
            for (column, &index) in indices.iter().enumerate() {
                line[column * layout.scale..(column + 1) * layout.scale].fill(scene.color(index));
            }
        }
        source_row += 1;
    }
    if let Some(fill) = layout.progress_rect(&scene, permille) {
        for y in fill.y..fill.y + fill.height {
            target.row_mut(y)[fill.x..fill.x + fill.width].fill(scene.fill_color());
        }
    }
    Ok(())
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
