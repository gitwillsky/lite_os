use crate::color::{Color, div255, over};
use crate::geometry::Rect;

/// 目标像素缓冲：行优先的 `0xAARRGGBB`，所有操作都被裁剪矩形限制。
pub struct Surface<'a> {
    pixels: &'a mut [u32],
    width: i32,
    height: i32,
    stride: usize,
    clip: Rect,
}

/// 源像素缓冲（预乘 ARGB），供 [`Surface::blit`] 读取。
#[derive(Clone, Copy)]
pub struct Image<'a> {
    pub pixels: &'a [u32],
    pub width: i32,
    pub height: i32,
    pub stride: usize,
}

/// 8 位覆盖率蒙版，供 [`Surface::draw_mask`] 以单色着色；字形光栅化的输出格式。
#[derive(Clone, Copy)]
pub struct Mask<'a> {
    pub coverage: &'a [u8],
    pub width: i32,
    pub height: i32,
    pub stride: usize,
}

fn fits(len: usize, width: i32, height: i32, stride: usize) -> bool {
    width >= 0
        && height >= 0
        && stride >= width as usize
        && (height == 0 || len >= stride * (height as usize - 1) + width as usize)
}

impl<'a> Surface<'a> {
    /// 在 `pixels` 上建立 `width`×`height` 的绘制面，`stride` 以像素为单位。
    ///
    /// 尺寸为负、`stride` 小于宽度或缓冲不足以容纳最后一行时返回 `None`，之后所有索引都在界内。
    pub fn new(pixels: &'a mut [u32], width: i32, height: i32, stride: usize) -> Option<Self> {
        fits(pixels.len(), width, height, stride).then_some(Self {
            pixels,
            width,
            height,
            stride,
            clip: Rect::new(0, 0, width, height),
        })
    }

    pub fn width(&self) -> i32 {
        self.width
    }

    pub fn height(&self) -> i32 {
        self.height
    }

    pub fn clip(&self) -> Rect {
        self.clip
    }

    /// 把裁剪矩形设为 `rect` 与整个面的交集，返回旧裁剪矩形以便调用者恢复。
    pub fn set_clip(&mut self, rect: Rect) -> Rect {
        let previous = self.clip;
        self.clip = rect.intersect(Rect::new(0, 0, self.width, self.height));
        previous
    }

    /// 读取像素；越出绘制面返回 `None`（不受裁剪影响）。
    pub fn pixel(&self, x: i32, y: i32) -> Option<Color> {
        let inside = (0..self.width).contains(&x) && (0..self.height).contains(&y);
        inside.then(|| Color(self.pixels[y as usize * self.stride + x as usize]))
    }

    /// `rect` 经裁剪后每一行的可写切片，连同该行的 `(x, y)` 起点。
    fn visible_rows(&mut self, rect: Rect) -> impl Iterator<Item = (i32, i32, &mut [u32])> {
        let area = rect.intersect(self.clip);
        let stride = self.stride;
        let width = area.width as usize;
        let start = if area.is_empty() {
            0
        } else {
            area.y as usize * stride + area.x as usize
        };
        let rows = if area.is_empty() {
            0
        } else {
            area.height as usize
        };
        let tail = if rows == 0 {
            &mut [][..]
        } else {
            &mut self.pixels[start..]
        };
        tail.chunks_mut(stride)
            .take(rows)
            .enumerate()
            .map(move |(row, chunk)| (area.x, area.y + row as i32, &mut chunk[..width]))
    }

    pub fn fill_rect(&mut self, rect: Rect, color: Color) {
        for (_, _, row) in self.visible_rows(rect) {
            row.fill(color.0);
        }
    }

    /// 绘制一圈线宽为 `width` 像素的斜边框：上、左边用 `top_left`，下、右边用 `bottom_right`，内部不动。
    ///
    /// 右上与左下的 `width` × `width` 斜接方块归 `bottom_right`，与 Win32 `DrawEdge` 一致；
    /// 线宽取整数 scale 时，每条逻辑 1 像素线的斜接方块仍是整块，不会在物理像素层面出现台阶。
    pub fn bevel(&mut self, rect: Rect, top_left: Color, bottom_right: Color, width: i32) {
        let width = width.min(rect.width / 2).min(rect.height / 2);
        if width <= 0 {
            return;
        }
        self.fill_rect(Rect::new(rect.x, rect.y, rect.width, width), top_left);
        self.fill_rect(Rect::new(rect.x, rect.y, width, rect.height), top_left);
        self.fill_rect(
            Rect::new(rect.x, rect.bottom() - width, rect.width, width),
            bottom_right,
        );
        self.fill_rect(
            Rect::new(rect.right() - width, rect.y, width, rect.height),
            bottom_right,
        );
    }

    /// 在 `rect` 上画从 `left` 到 `right` 的水平线性渐变；渐变跨越整个 `rect`，与裁剪无关。
    pub fn gradient_horizontal(&mut self, rect: Rect, left: Color, right: Color) {
        let span = (rect.width - 1).max(1) as u32;
        for (x, _, row) in self.visible_rows(rect) {
            for (column, pixel) in row.iter_mut().enumerate() {
                let t = ((x + column as i32 - rect.x) as u32).min(span);
                *pixel = left.lerp(right, t, span).0;
            }
        }
    }

    /// 以 `cell`×`cell` 像素为单元、以 `rect` 左上角为原点的棋盘：原点单元用 `even`，相邻用 `odd`。
    ///
    /// Win32 经典滚动条轨道是 1 逻辑像素的 50% 抖动，缩放 N 时 `cell` 取 N，图案保持逻辑比例。
    pub fn checker(&mut self, rect: Rect, cell: i32, even: Color, odd: Color) {
        let cell = cell.max(1);
        for (x, y, row) in self.visible_rows(rect) {
            for (column, pixel) in row.iter_mut().enumerate() {
                let parity = ((x + column as i32 - rect.x) / cell + (y - rect.y) / cell) & 1;
                *pixel = if parity == 0 { even.0 } else { odd.0 };
            }
        }
    }

    /// 反转 RGB 通道（异或 `0x00FFFFFF`），再次调用恢复原样。用于"拖动时不显示内容"的框线。
    pub fn invert(&mut self, rect: Rect) {
        for (_, _, row) in self.visible_rows(rect) {
            row.iter_mut().for_each(|pixel| *pixel ^= 0x00FF_FFFF);
        }
    }

    /// 反转 `rect` 边缘 1 个 `cell` 宽度的点状框：沿边每隔一个单元反转一个，焦点虚线框的绘制方式。
    ///
    /// 点的奇偶按绝对坐标计算，因此同一矩形调用两次即可擦除。
    pub fn invert_dotted_frame(&mut self, rect: Rect, cell: i32) {
        let cell = cell.max(1);
        for (x, y, row) in self.visible_rows(rect) {
            for (column, pixel) in row.iter_mut().enumerate() {
                let px = x + column as i32;
                let on_edge = px < rect.x + cell
                    || px >= rect.right() - cell
                    || y < rect.y + cell
                    || y >= rect.bottom() - cell;
                if on_edge && (px / cell + y / cell) & 1 == 0 {
                    *pixel ^= 0x00FF_FFFF;
                }
            }
        }
    }

    /// 把预乘 ARGB 图像按整数倍 `scale` 最近邻放大后合成到 `(x, y)`。
    ///
    /// 不透明像素直接覆盖，全透明像素跳过；`scale` 小于 1 时不绘制。HiDPI 下缺少对应倍率素材时的唯一缩放方式。
    pub fn blit(&mut self, image: Image<'_>, x: i32, y: i32, scale: i32) {
        if scale < 1 || !fits(image.pixels.len(), image.width, image.height, image.stride) {
            return;
        }
        let target = Rect::new(
            x,
            y,
            image.width.saturating_mul(scale),
            image.height.saturating_mul(scale),
        );
        for (left, top, row) in self.visible_rows(target) {
            let source_y = ((top - y) / scale) as usize;
            let source = &image.pixels[source_y * image.stride..][..image.width as usize];
            for (column, pixel) in row.iter_mut().enumerate() {
                let value = source[((left + column as i32 - x) / scale) as usize];
                match value >> 24 {
                    0 => {}
                    255 => *pixel = value,
                    _ => *pixel = over(value, *pixel),
                }
            }
        }
    }

    /// 用 `color`（不透明或预乘）按覆盖率蒙版着色，左上角放在 `(x, y)`。
    pub fn draw_mask(&mut self, mask: Mask<'_>, x: i32, y: i32, color: Color) {
        if !fits(mask.coverage.len(), mask.width, mask.height, mask.stride) {
            return;
        }
        for (left, top, row) in self.visible_rows(Rect::new(x, y, mask.width, mask.height)) {
            let source = &mask.coverage[(top - y) as usize * mask.stride..][..mask.width as usize];
            for (column, pixel) in row.iter_mut().enumerate() {
                let coverage = u32::from(source[(left + column as i32 - x) as usize]);
                match coverage {
                    0 => {}
                    255 if color.alpha() == 255 => *pixel = color.0,
                    _ => *pixel = over(scale_color(color, coverage), *pixel),
                }
            }
        }
    }
}

/// 把预乘颜色整体乘以 `coverage / 255`。
fn scale_color(color: Color, coverage: u32) -> u32 {
    let mut out = 0;
    for shift in [0, 8, 16, 24] {
        out |= div255((color.0 >> shift & 0xFF) * coverage) << shift;
    }
    out
}
