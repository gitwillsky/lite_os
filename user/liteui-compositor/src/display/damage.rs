use core::ptr;

use crate::{
    ffi::{self, DrmClip},
    scene::{Rect, Scene},
};

use super::Display;

const MAX_DAMAGE_RECTS: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq)]
/// @description presenter request 的 framebuffer role；不泄漏 reactor scene ownership。
pub(crate) enum DamageTarget {
    /// 当前 scanout buffer 上的局部更新。
    Active,
    /// inactive buffer 上为后续 page flip 准备的更新。
    Flip,
}

#[derive(Clone, Copy)]
/// @description 可跨固定 SPSC seam 复制、且不借用 Display/Scene 的 DIRTYFB request。
pub(crate) struct DamageRequest {
    fd: i32,
    framebuffer_id: u32,
    clips: [DrmClip; MAX_DAMAGE_RECTS],
    clip_count: u32,
    buffer: u8,
    target: DamageTarget,
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

    /// @description 合并一个矩形；相邻/相交区域折叠，容量耗尽时退化为 union。
    /// @param rectangle scene 坐标中的半开 damage 区域。
    pub(super) fn push(&mut self, mut rectangle: Rect) {
        if rectangle.x1 >= rectangle.x2 || rectangle.y1 >= rectangle.y2 {
            return;
        }
        // 1. 先合并相交或相邻区域，避免同一帧重复传输重叠像素。
        let mut index = 0;
        while index < self.count {
            if touches(self.rectangles[index], rectangle) {
                rectangle = rectangle.union(self.rectangles[index]);
                self.count -= 1;
                self.rectangles[index] = self.rectangles[self.count];
                index = 0;
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
        // 3. 固定数组耗尽时合并为单一区域，保持无分配且不丢失 damage。
        for current in &self.rectangles {
            rectangle = rectangle.union(*current);
        }
        self.rectangles[0] = rectangle;
        self.count = 1;
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

    /// @description 返回本次同步是否为 geometry page flip 的 inactive buffer 准备。
    /// @return inactive flip target 返回 true，active pointer target 返回 false。
    pub(crate) fn prepares_flip(&self) -> bool {
        self.target == DamageTarget::Flip
    }
}

impl Display {
    /// @description 将 scene damage 累积到所有 buffer，并撤销旧的 flip-ready 事实。
    /// @param rectangle scene 坐标中的半开 damage 区域。
    pub(crate) fn damage(&mut self, rectangle: Rect) {
        for buffer in self.buffers.iter_mut().flatten() {
            buffer.damage.push(rectangle);
            buffer.prepared_for_flip = false;
        }
    }

    /// @description 判断当前 scanout buffer 是否存在待同步 damage。
    /// @return active buffer 有 damage 时返回 true。
    pub(crate) fn has_active_damage(&self) -> bool {
        self.buffers[self.front]
            .as_ref()
            .is_some_and(|buffer| !buffer.damage.is_empty())
    }

    /// @description 判断 inactive buffer 是否仍有未同步 damage 或已同步待重试 flip。
    /// @return 任一工作事实存在时返回 true。
    pub(crate) fn has_flip_work(&self) -> bool {
        self.buffers[self.front ^ 1]
            .as_ref()
            .is_some_and(|buffer| !buffer.damage.is_empty() || buffer.prepared_for_flip)
    }

    /// @description 渲染并摘下一个 immutable damage snapshot，交给唯一 presenter worker。
    /// @param scene reactor 当前 scene snapshot；worker 不借用或修改它。
    /// @param target active pointer update 或 inactive geometry flip preparation。
    /// @return 无 damage/flip pending 时为 None；否则返回完全自包含的固定 request。
    /// @errors buffer/inflight state 损坏或 clip 无法编码时返回 unit error。
    pub(crate) fn prepare_damage(
        &mut self,
        scene: &Scene,
        target: DamageTarget,
    ) -> Result<Option<DamageRequest>, ()> {
        if self.flip_pending {
            return Ok(None);
        }
        let index = match target {
            DamageTarget::Active => self.front,
            DamageTarget::Flip => self.front ^ 1,
        };
        let Some(buffer) = self.buffers[index].as_mut() else {
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
            buffer: index as u8,
            target,
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
        let index = usize::from(request.buffer);
        let Some(buffer) = self.buffers.get_mut(index).and_then(Option::as_mut) else {
            return Err(());
        };
        if buffer.framebuffer_id != request.framebuffer_id {
            return Err(());
        }
        let inflight = buffer.inflight.take().ok_or(())?;
        if error != 0 {
            buffer.damage.merge(inflight);
            buffer.prepared_for_flip = false;
            return if matches!(error, ffi::EBUSY | ffi::EINTR) {
                Ok(false)
            } else {
                Err(())
            };
        }
        if request.target == DamageTarget::Flip && buffer.damage.is_empty() {
            buffer.prepared_for_flip = true;
        }
        Ok(true)
    }

    /// @description 对已由 presenter 同步且未被新输入污染的 inactive buffer 提交 page flip。
    /// @return page flip 已异步排队为 true；尚未 prepared/仍有 damage/瞬时忙为 false。
    /// @errors buffer state 或不可恢复的 page-flip error 返回 unit error。
    pub(crate) fn present_flip(&mut self) -> Result<bool, ()> {
        if self.flip_pending {
            return Ok(false);
        }
        let back = self.front ^ 1;
        let Some(buffer) = self.buffers[back].as_mut() else {
            return Err(());
        };
        if !buffer.prepared_for_flip || !buffer.damage.is_empty() || buffer.inflight.is_some() {
            return Ok(false);
        }
        if unsafe {
            ffi::drmModePageFlip(
                self.fd,
                self.crtc_id,
                buffer.framebuffer_id,
                ffi::DRM_MODE_PAGE_FLIP_EVENT,
                ptr::null_mut(),
            )
        } < 0
        {
            return if matches!(ffi::errno(), ffi::EBUSY | ffi::EINTR | ffi::EINVAL) {
                Ok(false)
            } else {
                Err(())
            };
        }
        buffer.prepared_for_flip = false;
        self.flip_pending = true;
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

fn touches(first: Rect, second: Rect) -> bool {
    first.x1 <= second.x2 && second.x1 <= first.x2 && first.y1 <= second.y2 && second.y1 <= first.y2
}
