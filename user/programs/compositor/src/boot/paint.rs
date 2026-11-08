//! 启动画面的纯 CPU 绘制；scanout adapter 与进度状态不属于此 codec。
//!
//! 一帧分两部分：静态位图只在某个扫描缓冲第一次使用时整屏画一次；动画带与进度条每帧重画。
//! 动画带与进度条的内容只由（平移量、进度）决定，重画它们不依赖缓冲里上一帧的内容，
//! 所以双缓冲各自画过一次静态位图后，每帧只写这两块并只上报这两块的损伤。
use boot_scene::{Layout, ParseError, Rect, WIDTH};

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

/// 已校验的 `LBS2` 数据与预先解出的动画带调色板索引；动画每帧只按平移量取列，不再解码 RLE。
pub struct Scene {
    bytes: Vec<u8>,
    band: Vec<u8>,
}

impl Scene {
    /// 校验 `bytes` 并解出动画带各行的索引（整个启动阶段只做一次）。
    ///
    /// # Errors
    ///
    /// 位图格式非法。
    pub fn load(bytes: Vec<u8>) -> Result<Self, BootError> {
        let scene = boot_scene::Scene::parse(&bytes).map_err(BootError::Scene)?;
        let mut band = vec![0; WIDTH * scene.band_rect().height];
        let decoded = scene.band_indices(&mut band);
        debug_assert!(decoded, "buffer sized from the same header");
        Ok(Self { bytes, band })
    }

    fn parsed(&self) -> boot_scene::Scene<'_> {
        boot_scene::Scene::parse(&self.bytes).expect("validated when loaded")
    }
}

/// 一帧启动画面的可变部分。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    /// 动画带向右循环平移的位图像素数（见 [`boot_scene::band_offset`]）。
    pub offset: usize,
    /// 进度条填充（0..=1000）。
    pub permille: u16,
}

/// 把一帧画进 `target`，返回本次改写的屏幕矩形（同时就是相对前一帧的 KMS 损伤）。
///
/// # Parameters
///
/// - `target`: 尺寸与可写像素行一致的 CPU surface。
/// - `scene`: 已校验的启动画面。
/// - `frame`: 平移量与进度。
/// - `full`: 该缓冲还没有静态位图时为 true：整屏清黑并画位图；否则只重画动画带与进度条。
///
/// # Errors
///
/// 屏幕太小。
pub(crate) fn paint(
    target: &mut impl BootSurface,
    scene: &Scene,
    frame: Frame,
    full: bool,
) -> Result<Vec<Rect>, BootError> {
    let parsed = scene.parsed();
    let layout = Layout::fit(target.width(), target.height()).ok_or(BootError::ScreenTooSmall)?;
    if full {
        for row in 0..target.height() {
            target.row_mut(row).fill(0);
        }
        let mut indices = [0u8; WIDTH];
        let mut rows = parsed.rows();
        let mut source_row = 0;
        while rows.next_row(&mut indices) {
            let colors = indices.map(|index| parsed.color(index));
            for repeat in 0..layout.scale {
                let line = target.row_mut(layout.y + source_row * layout.scale + repeat);
                scale_row(
                    &mut line[layout.x..layout.x + WIDTH * layout.scale],
                    &colors,
                    0,
                );
            }
            source_row += 1;
        }
    }
    // 动画带：整行宽循环平移，带内每个源行先换算成颜色，再按平移量写入放大后的屏幕行。
    let band = layout.band_rect(&parsed);
    for (source_row, indices) in scene.band.as_chunks::<WIDTH>().0.iter().enumerate() {
        let colors = indices.map(|index| parsed.color(index));
        for repeat in 0..layout.scale {
            let line = target.row_mut(band.y + source_row * layout.scale + repeat);
            scale_row(
                &mut line[band.x..band.x + band.width],
                &colors,
                frame.offset,
            );
        }
    }
    let progress = layout.progress_rect(&parsed, frame.permille);
    if let Some(fill) = progress {
        for y in fill.y..fill.y + fill.height {
            target.row_mut(y)[fill.x..fill.x + fill.width].fill(parsed.fill_color());
        }
    }
    Ok(if full {
        vec![Rect {
            x: 0,
            y: 0,
            width: target.width(),
            height: target.height(),
        }]
    } else {
        // 进度只增不减，填充矩形覆盖了相对前一帧的全部变化。
        [Some(band), progress].into_iter().flatten().collect()
    })
}

/// 把一行源颜色向右循环平移 `offset` 后按整数倍放大写入 `line`（长度为 `WIDTH * scale`）。
fn scale_row(line: &mut [u32], colors: &[u32; WIDTH], offset: usize) {
    let scale = line.len() / WIDTH;
    let offset = offset % WIDTH;
    for (column, pixels) in line.chunks_exact_mut(scale).enumerate() {
        pixels.fill(colors[(column + WIDTH - offset) % WIDTH]);
    }
}
