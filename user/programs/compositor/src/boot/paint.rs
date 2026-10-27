//! 启动画面的纯 CPU 绘制；scanout adapter 与进度状态不属于此 codec。
use boot_scene::{Layout, ParseError, Scene, WIDTH};

/// 启动画面使用的 CPU 可写像素行；同一画面直接写入选中后端的扫描资源。
/// 两种后端都保证行宽、行数与 row_mut 一致，不额外分配全屏过渡缓冲。
pub(crate) trait BootSurface {
    fn width(&self) -> usize;
    fn height(&self) -> usize;
    fn row_mut(&mut self, row: usize) -> &mut [u32];
}
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
/// - `target`: 尺寸与可写像素行一致的 CPU surface。
/// - `bytes`: `LBS1` 文件内容。
/// - `permille`: 进度条填充（0..=1000）；接管内核画面时用 [`boot_scene::KERNEL_PROGRESS_PERMILLE`]。
///
/// # Returns
///
/// 成功绘制整屏后返回 unit。
///
/// # Errors
///
/// 位图格式非法或屏幕太小。
pub(crate) fn paint(
    target: &mut impl BootSurface,
    bytes: &[u8],
    permille: u16,
) -> Result<(), BootError> {
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
