use crate::{
    ffi::{self, DrmClip},
    scene::{Rect, Scene},
};

use super::Display;
use crate::diagnostics::FrameMetrics;

const MAX_DAMAGE_RECTS: usize = 32;

#[derive(Clone, Copy)]
/// @description 可跨固定 SPSC seam 复制、且不借用 Display/Scene 的 DIRTYFB request。
pub(crate) struct DamageRequest {
    fd: i32,
    framebuffer_id: u32,
    clips: [DrmClip; MAX_DAMAGE_RECTS],
    clip_count: u32,
}

#[derive(Clone, Copy)]
/// @description 固定容量、无分配的 damage accumulator；容量耗尽时保守合并而不丢区域。
pub(super) struct DamageSet {
    rectangles: [Rect; MAX_DAMAGE_RECTS],
    count: usize,
}

impl DamageSet {
    /// @description 无待同步区域的初始 accumulator。
    pub(super) const EMPTY: Self = Self {
        rectangles: [Rect {
            x1: 0,
            y1: 0,
            x2: 0,
            y2: 0,
        }; MAX_DAMAGE_RECTS],
        count: 0,
    };

    /// @description 合并一个矩形；先消除 containment，容量耗尽时选择最小 overdraw。
    /// @param rectangle scene 坐标中的半开 damage 区域。
    pub(super) fn push(&mut self, rectangle: Rect) {
        if rectangle.x1 >= rectangle.x2 || rectangle.y1 >= rectangle.y2 {
            return;
        }
        // 1. containment 不增加传输面积；普通 overlap 不能合并，否则连续光标轨迹
        // 会递归膨胀为覆盖整条路径的包围盒。
        let mut index = 0;
        while index < self.count {
            if contains(self.rectangles[index], rectangle) {
                return;
            }
            if contains(rectangle, self.rectangles[index]) {
                self.count -= 1;
                self.rectangles[index] = self.rectangles[self.count];
            } else {
                index += 1;
            }
        }
        // 2. 有空位时保留离散矩形，pointer 只传输旧、新光标覆盖的像素。
        if self.count < MAX_DAMAGE_RECTS {
            self.rectangles[self.count] = rectangle;
            self.count += 1;
            return;
        }
        // 3. 只有 UAPI clip 容量确实耗尽时才引入 overdraw，并选择面积膨胀最小的
        // existing rectangle；全量 union 会把一次局部输入放大到整屏。
        let mut selected = 0;
        let mut selected_overdraw = usize::MAX;
        let incoming_area = area(rectangle);
        for (index, current) in self.rectangles.iter().copied().enumerate() {
            let union = rectangle.union(current);
            let overdraw = area(union).saturating_sub(area(current).saturating_add(incoming_area));
            if overdraw < selected_overdraw {
                selected = index;
                selected_overdraw = overdraw;
            }
        }
        self.rectangles[selected] = self.rectangles[selected].union(rectangle);
    }

    fn clear(&mut self) {
        self.count = 0;
    }

    fn merge(&mut self, other: Self) {
        for rectangle in other.rectangles().iter().copied() {
            self.push(rectangle);
        }
    }

    /// @description 判断是否不存在待同步区域。
    /// @return 无区域时返回 true。
    pub(super) fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn rectangles(&self) -> &[Rect] {
        &self.rectangles[..self.count]
    }
}

impl DamageRequest {
    pub(crate) fn metrics(&self) -> FrameMetrics {
        let pixels = self.clips[..self.clip_count as usize]
            .iter()
            .map(|clip| u64::from(clip.x2 - clip.x1).saturating_mul(u64::from(clip.y2 - clip.y1)))
            .fold(0u64, u64::saturating_add);
        FrameMetrics {
            clips: self.clip_count,
            pixels,
        }
    }

    /// @description 在 presenter worker 上执行标准 blocking DRM DIRTYFB ioctl。
    /// @return 成功为零；失败为 worker thread-local errno。
    pub(crate) fn execute(&mut self) -> i32 {
        if unsafe {
            ffi::drmModeDirtyFB(
                self.fd,
                self.framebuffer_id,
                self.clips.as_mut_ptr(),
                self.clip_count,
            )
        } >= 0
        {
            0
        } else {
            ffi::errno()
        }
    }
}

impl Display {
    /// @description 将 scene damage 累积到唯一持久 scanout buffer。
    /// @param rectangle scene 坐标中的半开 damage 区域。
    pub(crate) fn damage(&mut self, rectangle: Rect) {
        if let Some(buffer) = self.buffer.as_mut() {
            buffer.damage.push(rectangle);
        }
    }

    /// @description 判断唯一 scanout buffer 是否存在待同步 damage。
    /// @return buffer 有 damage 时返回 true。
    pub(crate) fn has_damage(&self) -> bool {
        self.buffer
            .as_ref()
            .is_some_and(|buffer| !buffer.damage.is_empty())
    }

    /// @description 渲染并摘下一个 immutable damage snapshot，交给唯一 presenter worker。
    /// @param scene reactor 当前 scene snapshot；worker 不借用或修改它。
    /// @return 无 damage 时为 None；否则返回完全自包含的固定 request。
    /// @errors buffer/inflight state 损坏或 clip 无法编码时返回 unit error。
    pub(crate) fn prepare_damage(&mut self, scene: &Scene) -> Result<Option<DamageRequest>, ()> {
        let Some(buffer) = self.buffer.as_mut() else {
            return Err(());
        };
        if buffer.damage.is_empty() {
            return Ok(None);
        }
        if buffer.inflight.is_some() {
            return Err(());
        }
        let damage = buffer.damage;
        let mut clips = [DrmClip::default(); MAX_DAMAGE_RECTS];
        for (index, rectangle) in damage.rectangles().iter().copied().enumerate() {
            clips[index] = clip(rectangle)?;
        }
        buffer.damage.clear();
        for rectangle in damage.rectangles().iter().copied() {
            scene.render(buffer.pixels, buffer.pitch, rectangle);
        }
        buffer.inflight = Some(damage);
        Ok(Some(DamageRequest {
            fd: self.fd,
            framebuffer_id: buffer.framebuffer_id,
            clips,
            clip_count: damage.count as u32,
        }))
    }

    /// @description 提交 presenter completion；失败 snapshot 无损合并回当前 damage。
    /// @param request presenter 原样返回的唯一在途 request。
    /// @param error DIRTYFB 成功为 0，失败为 worker thread-local errno。
    /// @return 成功 completion 为 true；瞬时失败已恢复 damage 并返回 false。
    /// @errors request/buffer owner 不匹配或不可恢复的 DIRTYFB error 返回 unit error。
    pub(crate) fn complete_damage(
        &mut self,
        request: DamageRequest,
        error: i32,
    ) -> Result<bool, ()> {
        let Some(buffer) = self.buffer.as_mut() else {
            return Err(());
        };
        if buffer.framebuffer_id != request.framebuffer_id {
            return Err(());
        }
        let inflight = buffer.inflight.take().ok_or(())?;
        if error != 0 {
            buffer.damage.merge(inflight);
            return if matches!(error, ffi::EBUSY | ffi::EINTR) {
                Ok(false)
            } else {
                Err(())
            };
        }
        Ok(true)
    }
}

fn clip(rectangle: Rect) -> Result<DrmClip, ()> {
    Ok(DrmClip {
        x1: u16::try_from(rectangle.x1).map_err(|_| ())?,
        y1: u16::try_from(rectangle.y1).map_err(|_| ())?,
        x2: u16::try_from(rectangle.x2).map_err(|_| ())?,
        y2: u16::try_from(rectangle.y2).map_err(|_| ())?,
    })
}

fn contains(outer: Rect, inner: Rect) -> bool {
    outer.x1 <= inner.x1 && outer.y1 <= inner.y1 && outer.x2 >= inner.x2 && outer.y2 >= inner.y2
}

fn area(rectangle: Rect) -> usize {
    rectangle
        .x2
        .saturating_sub(rectangle.x1)
        .saturating_mul(rectangle.y2.saturating_sub(rectangle.y1))
}
