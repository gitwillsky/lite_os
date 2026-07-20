//! 像素渲染：把终端 model 画进桌面握手传来的 DRM dumb buffer。
//!
//! 渲染核（`render_full` / `render_cells` / `CellRenderer` / `render_cursor`）搬自
//! console-session 的 `display.rs`，只依赖 `{pixels, pitch, width, height}` 像素视图；
//! modeset / ADDFB / SETCRTC / DIRTYFB 全部属于桌面，本模块不出现。

use core::{ffi::c_void, ptr, slice};

use display_proto::MAX_DAMAGE_RECTS;

use crate::{
    atlas::{self, Atlas, FontMetrics},
    ffi::{self, DrmDumbCreate, DrmDumbMap},
    model::{
        ATTR_BLINK, ATTR_BOLD, ATTR_DIM, ATTR_HIDDEN, ATTR_INVERSE, ATTR_UNDERLINE, Cell, Grid,
        Model,
    },
};

const BACKGROUND: u32 = 0x00101418;

/// 一块映射到本进程地址空间的 DRM dumb buffer（XRGB8888）。
///
/// `handle` 的所有权随 `CREATE_SURFACE` 转移给桌面，由桌面最终 `DESTROY_DUMB`；
/// 本进程只持有 mmap 视图，`Drop` 仅 `munmap`。
pub struct Surface {
    pixels: *mut u32,
    size: usize,
    pitch: usize,
    width: usize,
    height: usize,
}

/// 一次 `present` 产出的 damage clip 集合（`{x1, y1, x2, y2}` 半开矩形）。
pub struct Damage {
    pub rects: [[u16; 4]; MAX_DAMAGE_RECTS],
    pub count: usize,
}

impl Surface {
    /// 在共享 DRM fd 上 `CREATE_DUMB` + `MAP_DUMB` + `mmap` 创建 `width`×`height`
    /// 的 32bpp dumb buffer，返回像素视图与 GEM handle。
    ///
    /// 失败时返回 `None`（不销毁已建 handle：进程随即退出，handle 由桌面随连接
    /// 生命周期回收）。`pitch` 不足 `width * 4` 或 `size` 容纳不下整幅时视为驱动
    /// 契约违约，同样返回 `None`。
    pub fn create(drm_fd: i32, width: u32, height: u32) -> Option<(Self, u32)> {
        let mut create = DrmDumbCreate {
            width,
            height,
            bpp: 32,
            ..DrmDumbCreate::default()
        };
        if unsafe {
            ffi::ioctl(
                drm_fd,
                ffi::DRM_IOCTL_MODE_CREATE_DUMB,
                (&mut create as *mut DrmDumbCreate).cast(),
            )
        } < 0
        {
            return None;
        }
        let size = usize::try_from(create.size).ok()?;
        let pitch = usize::try_from(create.pitch).ok()?;
        let required = pitch.checked_mul(usize::try_from(height).ok()?)?;
        if pitch < usize::try_from(width).ok()? * 4 || required > size {
            return None;
        }
        let mut map = DrmDumbMap {
            handle: create.handle,
            ..DrmDumbMap::default()
        };
        if unsafe {
            ffi::ioctl(
                drm_fd,
                ffi::DRM_IOCTL_MODE_MAP_DUMB,
                (&mut map as *mut DrmDumbMap).cast(),
            )
        } < 0
        {
            return None;
        }
        let pixels = unsafe {
            ffi::mmap(
                ptr::null_mut(),
                size,
                ffi::PROT_READ | ffi::PROT_WRITE,
                ffi::MAP_SHARED,
                drm_fd,
                map.offset as i64,
            )
        };
        if pixels as usize == usize::MAX {
            return None;
        }
        Some((
            Self {
                pixels: pixels.cast(),
                size,
                pitch,
                width: usize::try_from(width).ok()?,
                height: usize::try_from(height).ok()?,
            },
            create.handle,
        ))
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: pixels 是 create() 中成功 mmap 的 size 字节映射，本结构是唯一 owner。
        unsafe { ffi::munmap(self.pixels.cast::<c_void>(), self.size) };
    }
}

/// 整幅重绘：清背景后画出全部 cell 与光标，用于 surface 创建后的首帧。
///
/// `focused` 为 false 时不画光标（桌面尚未聚焦本窗口）。
pub fn render_full<G: Grid>(surface: &mut Surface, grid: &G, atlas: &Atlas, metrics: FontMetrics, focused: bool) {
    for row in 0..surface.height {
        let pixels = unsafe {
            slice::from_raw_parts_mut(
                (surface.pixels as *mut u8)
                    .add(row * surface.pitch)
                    .cast::<u32>(),
                surface.width,
            )
        };
        pixels.fill(BACKGROUND);
    }
    for row in 0..grid.rows() {
        render_cells(surface, grid, atlas, metrics, row, 0, grid.columns());
    }
    if focused {
        render_cursor(surface, grid, metrics);
    }
}

/// 增量重绘：渲染 model 的脏行，返回需要经 `COMMIT` 上报的 damage clip。
///
/// 行 dirty span 逐个合成一个 clip；clip 数达到 [`MAX_DAMAGE_RECTS`] 且行数超过
/// 该上限时坍缩为单个 union 矩形（对齐 console-session `present()` 的行为）。
/// 无脏行时返回 `None`，调用方不应发送 `COMMIT`。
pub fn present(
    surface: &mut Surface,
    model: &mut Model,
    atlas: &Atlas,
    metrics: FontMetrics,
    focused: bool,
) -> Option<Damage> {
    let mut damage = Damage {
        rects: [[0; 4]; MAX_DAMAGE_RECTS],
        count: 0,
    };
    let mut union = None::<(usize, usize, usize, usize)>;
    for row in 0..model.rows() {
        let Some((first, end)) = model.dirty_span(row) else {
            continue;
        };
        render_cells(surface, model, atlas, metrics, row, first, end);
        let cell_width = metrics.width();
        let cell_height = metrics.height();
        let rectangle = (
            first * cell_width,
            row * cell_height,
            (end * cell_width).min(surface.width),
            ((row + 1) * cell_height).min(surface.height),
        );
        union = Some(match union {
            None => rectangle,
            Some((x1, y1, x2, y2)) => (
                x1.min(rectangle.0),
                y1.min(rectangle.1),
                x2.max(rectangle.2),
                y2.max(rectangle.3),
            ),
        });
        if damage.count < MAX_DAMAGE_RECTS {
            damage.rects[damage.count] = clip(rectangle);
            damage.count += 1;
        }
    }
    union?;
    if focused {
        render_cursor(surface, model, metrics);
    }
    if damage.count == MAX_DAMAGE_RECTS && model.rows() > MAX_DAMAGE_RECTS {
        damage.rects[0] = clip(union.unwrap());
        damage.count = 1;
    }
    for row in 0..model.rows() {
        model.clear_dirty(row);
    }
    Some(damage)
}

fn render_cells<G: Grid>(
    surface: &mut Surface,
    grid: &G,
    atlas: &Atlas,
    metrics: FontMetrics,
    row: usize,
    first: usize,
    end: usize,
) {
    let renderer = CellRenderer {
        atlas,
        metrics,
        reverse_screen: grid.reverse_screen(),
        blink_visible: grid.blink_visible(),
    };
    for column in first..end {
        renderer.render(surface, row, column, grid.cell(row, column));
    }
}

/// Immutable facts shared by every cell painted in one grid span.
struct CellRenderer<'a> {
    atlas: &'a Atlas,
    metrics: FontMetrics,
    reverse_screen: bool,
    blink_visible: bool,
}

impl CellRenderer<'_> {
    fn render(&self, surface: &mut Surface, row: usize, column: usize, cell: Cell) {
        let (mut foreground, mut background) = (cell.foreground, cell.background);
        if (cell.attributes & ATTR_INVERSE != 0) ^ self.reverse_screen {
            core::mem::swap(&mut foreground, &mut background);
        }
        if cell.attributes & ATTR_HIDDEN != 0
            || cell.attributes & ATTR_BLINK != 0 && !self.blink_visible
        {
            foreground = background;
        }
        if cell.attributes & ATTR_DIM != 0 {
            foreground = (foreground & 0xfefefe) >> 1;
        }
        let glyph = self
            .atlas
            .glyph(cell.codepoint, cell.attributes & ATTR_BOLD != 0);
        let cell_width = self.metrics.width();
        let cell_height = self.metrics.height();
        for y in 0..cell_height {
            let pixel_y = row * cell_height + y;
            if pixel_y >= surface.height {
                break;
            }
            let pixels = unsafe {
                slice::from_raw_parts_mut(
                    (surface.pixels as *mut u8)
                        .add(pixel_y * surface.pitch)
                        .cast::<u32>()
                        .add(column * cell_width),
                    cell_width.min(surface.width.saturating_sub(column * cell_width)),
                )
            };
            for (x, pixel) in pixels.iter_mut().enumerate() {
                let alpha = if cell.attributes & ATTR_UNDERLINE != 0 && y + 3 >= cell_height {
                    255
                } else {
                    glyph[y * cell_width + x]
                };
                *pixel = atlas::blend(background, foreground, alpha);
            }
        }
    }
}

fn render_cursor<G: Grid>(surface: &mut Surface, grid: &G, metrics: FontMetrics) {
    let Some((row, column)) = grid.cursor() else {
        return;
    };
    let y = (row + 1) * metrics.height() - 3;
    let x = column * metrics.width();
    for offset_y in 0..3 {
        if y + offset_y >= surface.height || x >= surface.width {
            continue;
        }
        let pixels = unsafe {
            slice::from_raw_parts_mut(
                (surface.pixels as *mut u8)
                    .add((y + offset_y) * surface.pitch)
                    .cast::<u32>()
                    .add(x),
                metrics.width().min(surface.width - x),
            )
        };
        pixels.fill(0x00f8fafc);
    }
}

fn clip(rectangle: (usize, usize, usize, usize)) -> [u16; 4] {
    // 各分量已 clamp 到 surface 尺寸（1280×768，远小于 u16::MAX），转换不溢出。
    [
        rectangle.0 as u16,
        rectangle.1 as u16,
        rectangle.2 as u16,
        rectangle.3 as u16,
    ]
}
