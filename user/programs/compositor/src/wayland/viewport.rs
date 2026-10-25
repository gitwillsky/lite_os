//! `wp_viewporter`：surface 的源矩形裁剪与目标尺寸缩放。
//!
//! 合成器不在绘制时缩放：提交时把缓冲裁剪、最近邻缩放成"目标尺寸 × buffer scale"的内容，
//! 之后仍走整数倍放大的合成路径。原始内容保存在 [`ViewportState::raw`]，所以只改视口不换 buffer 的
//! 提交（Wine 全屏缩放的常见做法）也能重新取样。

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
    /// 已生效的源矩形 `(x, y, 宽, 高)`，缓冲像素；`None` 表示整个缓冲。
    pub source: Option<(f64, f64, f64, f64)>,
    /// 已生效的目标尺寸（surface 局部逻辑像素）；`None` 表示用源尺寸。
    pub destination: Option<(i32, i32)>,
    /// 下次 commit 要换上的值；外层 `Some` 表示有改动。
    pub pending: Option<Params>,
    /// 视口生效时保存的原始缓冲内容。
    pub raw: Option<Content>,
}

impl ViewportState {
    /// 视口是否在起作用。
    pub fn active(&self) -> bool {
        self.source.is_some() || self.destination.is_some()
    }

    fn pending_mut(&mut self) -> &mut Params {
        let current = (self.source, self.destination);
        self.pending.get_or_insert(current)
    }

    /// 取样出提交内容：把 `base` 的源矩形最近邻缩放成 `scale` 倍的目标尺寸。
    ///
    /// # Errors
    ///
    /// 源矩形越出缓冲，或没有目标尺寸且源尺寸不是整数，返回协议错误码（`out_of_buffer`/`bad_size`）。
    pub fn resample(&self, base: &Content, scale: i32) -> Result<Content, u32> {
        let (sx, sy, sw, sh) =
            self.source
                .unwrap_or((0.0, 0.0, base.width as f64, base.height as f64));
        if sx + sw > base.width as f64 + 1e-6 || sy + sh > base.height as f64 + 1e-6 {
            return Err(wp_viewport::Error::OutOfBuffer as u32);
        }
        let (dw, dh) = match self.destination {
            Some(size) => size,
            None if sw.fract() == 0.0 && sh.fract() == 0.0 => (sw as i32, sh as i32),
            None => return Err(wp_viewport::Error::BadSize as u32),
        };
        let scale = scale.max(1) as usize;
        let (width, height) = (dw.max(1) as usize * scale, dh.max(1) as usize * scale);
        let mut pixels = Vec::new();
        if pixels.try_reserve_exact(width * height).is_err() {
            return Err(wp_viewport::Error::BadSize as u32);
        }
        for y in 0..height {
            let source_y = (sy + (y as f64 + 0.5) * sh / height as f64) as usize;
            let row = source_y.min(base.height - 1) * base.width;
            for x in 0..width {
                let source_x = (sx + (x as f64 + 0.5) * sw / width as f64) as usize;
                pixels.push(base.pixels[row + source_x.min(base.width - 1)]);
            }
        }
        Ok(Content {
            width,
            height,
            pixels,
        })
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
                *surface.viewport.pending_mut() = (None, None);
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
        let pending = self.surface_mut(surface)?.viewport.pending_mut();
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
