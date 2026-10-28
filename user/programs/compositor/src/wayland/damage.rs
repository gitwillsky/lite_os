//! `wl_surface.damage`/`damage_buffer`：请求阶段按各自坐标系累计，commit 时换算成 buffer 像素矩形。
//! 合成器只按这些矩形重读 SHM 并上传纹理；损伤之外的像素沿用上一次提交（Wayland 允许的语义）。

use canvas::Rect;

/// 单个坐标系最多保留的矩形数；超过后合并成外接矩形，只会多读、不会漏读。
const LIMIT: usize = 16;

/// 一次 commit 之前累计的损伤。
#[derive(Default)]
pub(super) struct Damage {
    surface: Vec<Rect>,
    buffer: Vec<Rect>,
}

fn push(rects: &mut Vec<Rect>, rect: Rect) {
    if rect.is_empty() {
        return;
    }
    if rects.len() == LIMIT {
        let bounds = rects.iter().fold(rect, |bounds, rect| {
            let (x, y) = (bounds.x.min(rect.x), bounds.y.min(rect.y));
            Rect::new(
                x,
                y,
                bounds.right().max(rect.right()).saturating_sub(x),
                bounds.bottom().max(rect.bottom()).saturating_sub(y),
            )
        });
        rects.clear();
        rects.push(bounds);
    } else {
        rects.push(rect);
    }
}

impl Damage {
    pub(super) fn add_surface(&mut self, rect: Rect) {
        push(&mut self.surface, rect);
    }

    pub(super) fn add_buffer(&mut self, rect: Rect) {
        push(&mut self.buffer, rect);
    }

    /// 换算为裁剪到 `buffer` 尺寸内的 buffer 像素矩形。
    ///
    /// # Parameters
    ///
    /// - `buffer`: 新附加 buffer 的像素尺寸。
    /// - `scale`、`transform`: 本次提交生效的 buffer scale 与 transform。
    /// - `viewport`: 本次提交是否设置了 `wp_viewport` 源矩形或目标尺寸。
    ///
    /// # Returns
    ///
    /// 需要重读的矩形；surface 坐标损伤遇到 transform 或视口时退化为整幅 buffer。
    pub(super) fn resolve(
        self,
        buffer: (usize, usize),
        scale: i32,
        transform: i32,
        viewport: bool,
    ) -> Vec<Rect> {
        let bounds = Rect::new(0, 0, buffer.0 as i32, buffer.1 as i32);
        if !self.surface.is_empty() && (transform != 0 || viewport) {
            return vec![bounds];
        }
        let scaled = self.surface.into_iter().map(|rect| {
            Rect::new(
                rect.x.saturating_mul(scale),
                rect.y.saturating_mul(scale),
                rect.width.saturating_mul(scale),
                rect.height.saturating_mul(scale),
            )
        });
        let mut rects = Vec::new();
        for rect in scaled.chain(self.buffer) {
            push(&mut rects, rect.intersect(bounds));
        }
        rects
    }
}
