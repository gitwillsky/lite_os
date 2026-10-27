//! wp_viewporter 的标准采样元数据：先反解 buffer transform/scale，再解释逻辑源矩形与目标尺寸。
//! 不复制或重采样提交像素；CPU/GPU 使用同一仿射源坐标，视口改变可以复用原纹理。

use wayland_wire::{
    protocol::{Interface, wp_viewport, wp_viewporter},
    wire::{Fixed, ObjectId},
};

use super::{
    Content,
    client::{Client, Data, Entry, Fail},
};

/// 视口对象指向的 surface；surface 先销毁后为 `None`，此后除 destroy 外的请求是协议错误 `no_surface`。
pub struct Viewport {
    pub(super) surface: Option<ObjectId>,
}

/// 源矩形与目标尺寸这一对参数。
pub type Params = (Option<(f64, f64, f64, f64)>, Option<(i32, i32)>);

/// 一个 surface 当前生效与待生效的视口参数。
#[derive(Default)]
pub struct ViewportState {
    /// 绑定的 `wp_viewport` 对象；同一 surface 只允许一个。
    pub object: Option<ObjectId>,
    /// 下次 commit 要换上的值；外层 `Some` 表示有改动。
    pub pending: Option<Params>,
}

/// 协议错误归属：裸缓冲尺寸属于 wl_surface，裁剪错误属于 wp_viewport。
pub(super) enum GeometryError {
    BufferSize,
    Viewport(u32),
}

impl ViewportState {
    /// 只校验不依赖 buffer 的尺寸规则；空 buffer 也不能提交分数源尺寸而不指定目标。
    pub fn validate_size(params: Params) -> Result<(), u32> {
        if params.1.is_none()
            && params
                .0
                .is_some_and(|(_, _, width, height)| width.fract() != 0.0 || height.fract() != 0.0)
        {
            return Err(wp_viewport::Error::BadSize as u32);
        }
        Ok(())
    }

    /// 将已提交参数转换成逻辑尺寸与原始像素的仿射采样边界。
    /// 裸 buffer 不是 scale 的整数倍返回 BufferSize；越界源或分数源尺寸返回对应 viewport 错误。
    pub fn mapping(
        params: Params,
        base: &Content,
        scale: i32,
        transform: i32,
    ) -> Result<((i32, i32), canvas::ImageSource), GeometryError> {
        Self::validate_size(params).map_err(GeometryError::Viewport)?;
        let (width, height) = if transform & 1 != 0 {
            (base.height, base.width)
        } else {
            (base.width, base.height)
        };
        if params.0.is_none()
            && params.1.is_none()
            && (!width.is_multiple_of(scale as usize) || !height.is_multiple_of(scale as usize))
        {
            return Err(GeometryError::BufferSize);
        }
        let (sx, sy, sw, sh) = params.0.unwrap_or_else(|| {
            (
                0.0,
                0.0,
                width as f64 / f64::from(scale),
                height as f64 / f64::from(scale),
            )
        });
        if (sx + sw) * f64::from(scale) > width as f64
            || (sy + sh) * f64::from(scale) > height as f64
        {
            return Err(GeometryError::Viewport(
                wp_viewport::Error::OutOfBuffer as u32,
            ));
        }
        let size = params.1.unwrap_or((sw as i32, sh as i32));
        // 完整、未旋转的源保持精确单位坐标，让常规整数缩放复用 CPU 行复制快路径。
        if !base.y_inverted()
            && transform == 0
            && sx == 0.0
            && sy == 0.0
            && sw * f64::from(scale) == base.width as f64
            && sh * f64::from(scale) == base.height as f64
        {
            return Ok((
                size,
                canvas::ImageSource::full(base.width as i32, base.height as i32),
            ));
        }
        // set_buffer_transform 描述客户端已施加的变换；从显示点反查原 buffer 要按该正向变换取样。
        let point = |x: f64, y: f64| {
            let (u, v) = (
                x * f64::from(scale) / width as f64,
                y * f64::from(scale) / height as f64,
            );
            let (u, v) = match transform {
                0 => (u, v),
                1 => (v, 1.0 - u),
                2 => (1.0 - u, 1.0 - v),
                3 => (1.0 - v, u),
                4 => (1.0 - u, v),
                5 => (v, u),
                6 => (u, 1.0 - v),
                7 => (1.0 - v, 1.0 - u),
                _ => unreachable!("validated buffer transform"),
            };
            (
                u * base.width as f64,
                (if base.y_inverted() { 1.0 - v } else { v }) * base.height as f64,
            )
        };
        Ok((
            size,
            canvas::ImageSource {
                origin: point(sx, sy),
                right: point(sx + sw, sy),
                bottom: point(sx, sy + sh),
            },
        ))
    }
}

impl Client {
    pub(super) fn viewporter(
        &mut self,
        object: ObjectId,
        version: u32,
        request: wp_viewporter::Request,
    ) -> Result<(), Fail> {
        match request {
            wp_viewporter::Request::Destroy => Ok(()),
            wp_viewporter::Request::GetViewport { id, surface } => {
                if self.surface_mut(surface)?.viewport.object.is_some() {
                    return Err(Fail::new(
                        object,
                        wp_viewporter::Error::ViewportExists as u32,
                        "wl_surface already has a viewport",
                    ));
                }
                self.create(
                    id,
                    Interface::WpViewport,
                    version,
                    Data::Viewport(Viewport {
                        surface: Some(surface),
                    }),
                )?;
                self.surface_mut(surface)?.viewport.object = Some(id);
                Ok(())
            }
        }
    }

    pub(super) fn viewport(
        &mut self,
        object: ObjectId,
        request: wp_viewport::Request,
    ) -> Result<(), Fail> {
        let surface = match self.objects.get(&object.get()) {
            Some(Entry {
                data: Data::Viewport(viewport),
                ..
            }) => viewport.surface,
            _ => return Err(Fail::implementation(object)),
        };
        if matches!(request, wp_viewport::Request::Destroy) {
            if let Some(surface) = surface
                && let Ok(surface) = self.surface_mut(surface)
            {
                // 销毁视口等于取消源与目标，下次 commit 生效。
                surface.viewport.object = None;
                *surface.pending_viewport() = (None, None);
            }
            return Ok(());
        }
        let no_surface = || {
            Fail::new(
                object,
                wp_viewport::Error::NoSurface as u32,
                "the wl_surface of this wp_viewport no longer exists",
            )
        };
        let surface = surface
            .filter(|surface| self.surface_mut(*surface).is_ok())
            .ok_or_else(no_surface)?;
        let bad = |message: &str| {
            Fail::new(
                object,
                wp_viewport::Error::BadValue as u32,
                message.to_owned(),
            )
        };
        let pending = self.surface_mut(surface)?.pending_viewport();
        match request {
            wp_viewport::Request::SetSource {
                x,
                y,
                width,
                height,
            } => {
                let value = |fixed: Fixed| fixed.to_f64();
                let (x, y, width, height) = (value(x), value(y), value(width), value(height));
                if (x, y, width, height) == (-1.0, -1.0, -1.0, -1.0) {
                    pending.0 = None;
                } else if x < 0.0 || y < 0.0 || width <= 0.0 || height <= 0.0 {
                    return Err(bad("invalid source rectangle"));
                } else {
                    pending.0 = Some((x, y, width, height));
                }
            }
            wp_viewport::Request::SetDestination { width, height } => {
                if (width, height) == (-1, -1) {
                    pending.1 = None;
                } else if width <= 0 || height <= 0 {
                    return Err(bad("invalid destination size"));
                } else {
                    pending.1 = Some((width, height));
                }
            }
            wp_viewport::Request::Destroy => unreachable!("handled above"),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use canvas::{Image, Rect, Surface};

    #[test]
    fn all_buffer_transforms_sample_the_original_pixels() {
        let base = Content::new(2, 3, (1..=6).map(|n| 0xff000000 | n).collect());
        let expected = [
            [1, 2, 3, 4, 5, 6],
            [5, 3, 1, 6, 4, 2],
            [6, 5, 4, 3, 2, 1],
            [2, 4, 6, 1, 3, 5],
            [2, 1, 4, 3, 6, 5],
            [1, 3, 5, 2, 4, 6],
            [5, 6, 3, 4, 1, 2],
            [6, 4, 2, 5, 3, 1],
        ];
        for (transform, expected) in expected.into_iter().enumerate() {
            let Ok(((width, height), source)) =
                ViewportState::mapping((None, None), &base, 1, transform as i32)
            else {
                panic!("valid transform");
            };
            let mut pixels = [0; 6];
            Surface::new(&mut pixels, width, height, width as usize)
                .unwrap()
                .blit_sampled(
                    Image {
                        pixels: base.pixels().unwrap(),
                        width: 2,
                        height: 3,
                        stride: 2,
                    },
                    Rect::new(0, 0, width, height),
                    source,
                );
            assert_eq!(
                pixels.map(|n| n & 0xffffff),
                expected,
                "transform {transform}"
            );
        }
    }

    #[test]
    fn scale_and_fractional_viewport_crop_use_one_affine_source() {
        let base = Content::new(6, 4, (1..=24).map(|n| 0xff000000 | n).collect());
        let original = base.pixels().unwrap().as_ptr();
        let Ok(((width, height), source)) =
            ViewportState::mapping((Some((0.5, 0.0, 1.0, 1.0)), Some((1, 1))), &base, 2, 0)
        else {
            panic!("valid crop");
        };
        let mut pixel = [0];
        Surface::new(&mut pixel, width, height, 1)
            .unwrap()
            .blit_sampled(
                Image {
                    pixels: base.pixels().unwrap(),
                    width: 6,
                    height: 4,
                    stride: 6,
                },
                Rect::new(0, 0, width, height),
                source,
            );
        assert_eq!(pixel[0] & 0xffffff, 9);
        assert_eq!(base.pixels().unwrap().as_ptr(), original);
        let Ok((size, _)) = ViewportState::mapping((None, None), &base, 2, 0) else {
            panic!("valid scale");
        };
        assert_eq!(size, (3, 2));
    }
}
