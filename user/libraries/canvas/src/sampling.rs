//! 图像源坐标的仿射采样：裁剪、旋转、镜像与缩放共用同一组像素边界坐标。

/// 目标左上、右上、左下角对应的源图像像素边界；三点定义整个仿射采样平面。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImageSource {
    pub origin: (f64, f64),
    pub right: (f64, f64),
    pub bottom: (f64, f64),
}

impl ImageSource {
    /// 取整张 `width × height` 图像；坐标表示像素边界，采样时使用目标像素中心。
    pub fn full(width: i32, height: i32) -> Self {
        Self {
            origin: (0.0, 0.0),
            right: (f64::from(width), 0.0),
            bottom: (0.0, f64::from(height)),
        }
    }

    /// 将目标归一化坐标 `(u, v)` 映射到源像素坐标；输入通常在 0..1，方法不裁剪。
    pub fn point(self, u: f64, v: f64) -> (f64, f64) {
        (
            self.origin.0
                + u * (self.right.0 - self.origin.0)
                + v * (self.bottom.0 - self.origin.0),
            self.origin.1
                + u * (self.right.1 - self.origin.1)
                + v * (self.bottom.1 - self.origin.1),
        )
    }
}
