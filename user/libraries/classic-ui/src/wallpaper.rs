//! Windows 2000 桌面墙纸的合成规则；桌面外壳与“显示属性”预览共用，保证预览与实际一致。
use crate::{Bitmap, Painter};
use canvas::Rect;

/// 墙纸显示方式；对应 `HKCU\Control Panel\Desktop` 的 TileWallpaper 与 WallpaperStyle。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WallpaperStyle {
    /// 原始尺寸居中，超出屏幕的部分裁掉（Windows 2000 的出厂值）。
    #[default]
    Center,
    /// 从屏幕左上角起按原始尺寸重复铺满。
    Tile,
    /// 不保持宽高比，拉伸到整个屏幕。
    Stretch,
}

impl Bitmap {
    /// 按 Windows 2000 规则把墙纸画进代表整个屏幕的 `screen`（逻辑像素）。
    ///
    /// # 参数
    /// - `screen`：被表示屏幕在目标上的矩形；只在其中绘制。
    /// - `style`：显示方式。
    /// - `ratio`：`(screen.width, 被表示屏幕的逻辑宽)`。桌面传两个相同值；预览传缩小比例，
    ///   居中与平铺的原图尺寸按同一比例缩小，与 desk.cpl 的监视器预览一致。
    ///
    /// 只写墙纸覆盖的像素；背景色由调用者先填，居中时四周露出背景色。
    pub fn paint_wallpaper(
        &self,
        painter: &mut Painter<'_>,
        screen: Rect,
        style: WallpaperStyle,
        ratio: (i32, i32),
    ) {
        if screen.is_empty() || ratio.0 < 1 || ratio.1 < 1 {
            return;
        }
        let scaled = |length: i32| {
            (i64::from(length) * i64::from(ratio.0) / i64::from(ratio.1))
                .clamp(1, i64::from(i32::MAX)) as i32
        };
        let (width, height) = (scaled(self.width()), scaled(self.height()));
        painter.clipped(screen, |painter| match style {
            WallpaperStyle::Stretch => self.paint_scaled(painter, screen),
            WallpaperStyle::Center => self.paint_scaled(
                painter,
                Rect::new(
                    screen.x + (screen.width - width) / 2,
                    screen.y + (screen.height - height) / 2,
                    width,
                    height,
                ),
            ),
            WallpaperStyle::Tile => {
                // 局部重绘只遍历触及裁剪区的瓦片，起点仍锚在屏幕原点。
                let clip = painter.surface.clip();
                if clip.is_empty() {
                    return;
                }
                let scale = painter.scale();
                let first_x = screen.x + ((clip.x / scale - screen.x) / width) * width;
                let first_y = screen.y + ((clip.y / scale - screen.y) / height) * height;
                let end_x = painter.logical_ceil(clip.right());
                let end_y = painter.logical_ceil(clip.bottom());
                for y in (first_y..end_y).step_by(height as usize) {
                    for x in (first_x..end_x).step_by(width as usize) {
                        self.paint_scaled(painter, Rect::new(x, y, width, height));
                    }
                }
            }
        });
    }

    /// 最近邻缩放到逻辑矩形；与 GDI StretchBlt 的 COLORONCOLOR 模式一样不做插值。
    fn paint_scaled(&self, painter: &mut Painter<'_>, rect: Rect) {
        let target = painter.physical(rect);
        painter.surface.blit_scaled(self.image(), target);
    }
}
