use alloc::sync::Arc;

use crate::{
    drivers::{DisplayError, DisplayMode, DisplayRect, virtio_queue::VirtQueue},
    memory::DeviceBacking,
};

use super::wire::{
    VIRTIO_GPU_CMD_RESOURCE_CREATE_2D, VIRTIO_GPU_CMD_RESOURCE_UNREF,
    VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D, VIRTIO_GPU_FLAG_FENCE, VIRTIO_GPU_RESP_OK_NODATA,
    prepare_create, prepare_transfer, prepare_unref, read_u32, read_u64, write_u32, write_u64,
};
use super::{
    RuntimeOperation, RuntimeStage, VirtIOGpuDevice,
    resource::{full_rectangle, validate_backing},
};

pub(super) const MAX_DAMAGE_RECTS: usize = 32;
const DAMAGE_BATCH_CAPACITY: usize = 15;
const TRANSFER_REQUEST_SIZE: usize = 56;
const RESPONSE_SIZE: usize = 24;

#[derive(Clone, Copy)]
#[repr(C, align(64))]
struct DamageCommand {
    request: [u8; TRANSFER_REQUEST_SIZE],
    response: [u8; RESPONSE_SIZE],
    fence: u64,
    head: u16,
    live: bool,
}

impl DamageCommand {
    const EMPTY: Self = Self {
        request: [0; TRANSFER_REQUEST_SIZE],
        response: [0; RESPONSE_SIZE],
        fence: 0,
        head: 0,
        live: false,
    };
}

/// @description controlq 内无分配保存、批量发布并回收一次 DIRTYFB transaction。
pub(super) struct DamageTransition {
    rectangles: [DisplayRect; MAX_DAMAGE_RECTS],
    count: usize,
    next: usize,
    flush: DisplayRect,
    operation_fence: u64,
    batch_count: usize,
    completed: usize,
    // OWNER: request/response/head/fence 在同一固定槽中保持到 used-ring completion；
    // 若借用共享 command storage，多个同时在途的 TRANSFER 会互相覆盖 DMA 内容。
    commands: [DamageCommand; DAMAGE_BATCH_CAPACITY],
}

impl DamageTransition {
    /// @description 构造尚未承载 active damage operation 的固定 scratch。
    /// @return clip、batch 与 fence cursor 全部为空的 state。
    pub(super) const fn new() -> Self {
        Self {
            rectangles: [DisplayRect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            }; MAX_DAMAGE_RECTS],
            count: 0,
            next: 0,
            flush: DisplayRect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            },
            operation_fence: 0,
            batch_count: 0,
            completed: 0,
            commands: [DamageCommand::EMPTY; DAMAGE_BATCH_CAPACITY],
        }
    }

    /// @description 以已验证的 fixed clip copy 开始一次 damage operation。
    /// @param rectangles 不再访问 userspace 的完整固定副本。
    /// @param count 有效 prefix，必须位于 1..=MAX_DAMAGE_RECTS。
    pub(super) fn begin(&mut self, rectangles: [DisplayRect; MAX_DAMAGE_RECTS], count: usize) {
        assert!(
            (1..=MAX_DAMAGE_RECTS).contains(&count),
            "invalid VirtIO GPU damage clip count"
        );
        let mut left = u32::MAX;
        let mut top = u32::MAX;
        let mut right = 0u32;
        let mut bottom = 0u32;
        for rectangle in &rectangles[..count] {
            left = left.min(rectangle.x);
            top = top.min(rectangle.y);
            right = right.max(rectangle.x + rectangle.width);
            bottom = bottom.max(rectangle.y + rectangle.height);
        }
        self.rectangles = rectangles;
        self.count = count;
        self.next = 0;
        self.flush = DisplayRect {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        };
        self.operation_fence = 0;
        self.batch_count = 0;
        self.completed = 0;
    }

    /// @description 向同一 avail publication 批量加入最多 15 个相互独立的 TRANSFER。
    /// @param queue 当前无其他 pending command 的 controlq。
    /// @param next_fence adapter 唯一的 command fence allocator。
    /// @param operation_fence 已由 CREATE/UNREF 建立的 operation fence；首批可为空。
    /// @param mode target framebuffer 的 linear mode。
    /// @param resource_id 所有 transfer 共同更新的 resident resource。
    /// @return 整个 DIRTYFB transaction 的稳定 operation fence。
    /// @errors fence 空间耗尽或 request 编码失败返回 Device/InvalidRectangle。
    pub(super) fn publish_next(
        &mut self,
        queue: &mut VirtQueue,
        next_fence: &mut u64,
        operation_fence: Option<u64>,
        mode: DisplayMode,
        resource_id: u32,
    ) -> Result<u64, DisplayError> {
        assert!(self.batch_count == 0 && self.next < self.count);
        let free_descriptors = queue.free_descriptor_count();
        if free_descriptors < 4 {
            return Err(DisplayError::Device);
        }
        let capacity = usize::from(free_descriptors / 4).min(DAMAGE_BATCH_CAPACITY);
        let count = (self.count - self.next).min(capacity);
        let first_fence = *next_fence;
        let following_fence = first_fence
            .checked_add(count as u64)
            .ok_or(DisplayError::Device)?;

        // 1. 在接触 queue free-list 前完成全部 fallible 编码；缺少该阶段会让后续 clip
        //    的编码错误落在已摘下 descriptor 之后，而 split virtqueue 没有安全的局部
        //    rollback 入口。
        for index in 0..count {
            let command = &mut self.commands[index];
            let fence = first_fence + index as u64;
            prepare_transfer(
                &mut command.request,
                mode,
                self.rectangles[self.next + index],
                resource_id,
            )?;
            write_u32(&mut command.request, 0, VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D)
                .ok_or(DisplayError::Device)?;
            write_u32(&mut command.request, 4, VIRTIO_GPU_FLAG_FENCE)
                .ok_or(DisplayError::Device)?;
            write_u64(&mut command.request, 8, fence).ok_or(DisplayError::Device)?;
            command.response.fill(0);
            command.fence = fence;
            command.live = false;
        }

        // 2. 每个小于一页的 input/output 最坏各跨两个页；capacity 已按 free/4 收紧，
        //    因而固定 batch 从空闲计数到 descriptor chain 建立不再有可恢复错误。
        for command in &mut self.commands[..count] {
            let inputs = [&command.request[..]];
            let mut outputs = [&mut command.response[..]];
            command.head = queue
                .add_buffer(&inputs, &mut outputs)
                .expect("bounded GPU damage batch exhausted an empty controlq");
            command.live = true;
        }

        // 3. 所有 DMA storage 与 descriptor 已完整初始化后才一次发布整个 batch；caller
        //    只敲一次 doorbell，device 可乱序完成互不重叠或重叠的等价 transfer。
        for command in &self.commands[..count] {
            queue.add_to_avail(command.head);
        }
        *next_fence = following_fence;
        self.next += count;
        self.batch_count = count;
        self.completed = 0;
        self.operation_fence = operation_fence.unwrap_or(first_fence);
        Ok(self.operation_fence)
    }

    /// @description 验证并回收一个可乱序到达的 batch completion。
    /// @param head used ring 返回且已经由 VirtQueue 归还 free list 的 descriptor head。
    /// @return 当前 batch 全部完成时为 true。
    /// @errors head、response type 或 fence 不匹配返回 Device。
    pub(super) fn complete(&mut self, head: u16) -> Result<bool, DisplayError> {
        let command = self.commands[..self.batch_count]
            .iter_mut()
            .find(|command| command.live && command.head == head)
            .ok_or(DisplayError::Device)?;
        if read_u32(&command.response, 0) != Some(VIRTIO_GPU_RESP_OK_NODATA)
            || read_u32(&command.response, 4).is_none_or(|flags| flags & VIRTIO_GPU_FLAG_FENCE == 0)
            || read_u64(&command.response, 8) != Some(command.fence)
        {
            return Err(DisplayError::Device);
        }
        command.live = false;
        self.completed += 1;
        Ok(self.completed == self.batch_count)
    }

    /// @description 判断 controlq 当前是否由 damage batch 独占 pending descriptors。
    /// @return 至少一只 TRANSFER command 尚在 used-ring completion 前时返回 true。
    pub(super) fn batch_active(&self) -> bool {
        self.batch_count != 0
    }

    /// @description 结束已全部回收的 batch，使下一批或最终 FLUSH 可发布。
    pub(super) fn finish_batch(&mut self) {
        assert_eq!(self.completed, self.batch_count);
        self.batch_count = 0;
        self.completed = 0;
    }

    /// @description 判断 fixed clip prefix 是否仍有尚未发布的 TRANSFER。
    /// @return next cursor 尚未到达有效 clip count 时返回 true。
    pub(super) fn has_remaining(&self) -> bool {
        self.next < self.count
    }

    /// @description 返回覆盖全部 transfer clip 的单一最终 flush rectangle。
    /// @return begin 时由全部已验证 clip 计算出的最小 bounding rectangle。
    pub(super) fn flush_rectangle(&self) -> DisplayRect {
        self.flush
    }

    /// @description 返回首批 command 或更早 CREATE/UNREF 建立的 operation fence。
    /// @return 整个 DIRTYFB transaction 对 DRM 暴露的稳定 fence。
    pub(super) fn operation_fence(&self) -> u64 {
        self.operation_fence
    }

    /// @description 取消尚未进入 avail ring 的 damage state。
    pub(super) fn cancel(&mut self) {
        assert_eq!(self.batch_count, 0);
        self.count = 0;
        self.next = 0;
        self.operation_fence = 0;
    }
}

impl VirtIOGpuDevice {
    /// @description 验证 damage、预留两槽 resource，并提交首个 batch/创建 command。
    /// @param identity DRM framebuffer 的全局单调 identity。
    /// @param mode target framebuffer 的 canonical linear mode。
    /// @param backing target SG lifetime owner。
    /// @param rectangles 1..=32 个非空、位于 mode 内的 rectangle。
    /// @return 整个 DIRTYFB transaction 的稳定 operation fence。
    /// @errors identity/backing、rectangle、已有 operation 或 publication failure。
    pub(super) fn submit_resident_damage(
        &self,
        identity: u64,
        mode: DisplayMode,
        backing: Arc<DeviceBacking>,
        rectangles: &[DisplayRect],
    ) -> Result<u64, DisplayError> {
        if rectangles.is_empty() || rectangles.len() > MAX_DAMAGE_RECTS {
            return Err(DisplayError::InvalidRectangle);
        }
        validate_backing(mode, &backing)?;
        let mut control = self.control.lock();
        if control.pending.is_some() || control.operation.is_some() {
            return Err(DisplayError::WouldBlock);
        }
        let mut copied = [DisplayRect::default(); MAX_DAMAGE_RECTS];
        for (destination, rectangle) in copied.iter_mut().zip(rectangles.iter().copied()) {
            if rectangle.width == 0
                || rectangle.height == 0
                || rectangle
                    .x
                    .checked_add(rectangle.width)
                    .is_none_or(|right| right > mode.width)
                || rectangle
                    .y
                    .checked_add(rectangle.height)
                    .is_none_or(|bottom| bottom > mode.height)
            {
                return Err(DisplayError::InvalidRectangle);
            }
            *destination = rectangle;
        }
        let target = control.resources.prepare(identity, mode, backing)?;
        let resource_id = target.id(&control.resources);
        let damage_count = if target.is_new() {
            // 新 host resource 没有 framebuffer 的历史内容。若只上传本次局部 damage，
            // 随后的 scanout 会把未同步区域显示为黑色，并留下移动内容的轨迹。
            copied[0] = full_rectangle(mode);
            1
        } else {
            rectangles.len()
        };
        control.damage.begin(copied, damage_count);
        let prepared = (|| {
            if let Some(evicted) = target.evicted_id() {
                prepare_unref(&mut control.request, evicted)?;
                Ok(Some((
                    VIRTIO_GPU_CMD_RESOURCE_UNREF,
                    32,
                    RuntimeStage::UnrefEvicted,
                )))
            } else if target.is_new() {
                prepare_create(&mut control.request, mode, resource_id)?;
                Ok(Some((
                    VIRTIO_GPU_CMD_RESOURCE_CREATE_2D,
                    40,
                    RuntimeStage::Create,
                )))
            } else {
                Ok(None)
            }
        })();
        let first = match prepared {
            Ok(first) => first,
            Err(error) => {
                control.damage.cancel();
                let unpublished = control.resources.cancel(target);
                drop(control);
                drop(unpublished);
                return Err(error);
            }
        };
        control.operation = Some(RuntimeOperation::Damage(target));
        let result = if let Some((command, length, stage)) = first {
            self.publish_runtime(&mut control, command, length, None, stage)
        } else {
            self.publish_damage_batch(&mut control, None, mode, resource_id)
        };
        if result.is_err() {
            let target = match control.operation.take() {
                Some(RuntimeOperation::Damage(target)) => target,
                _ => unreachable!(),
            };
            control.damage.cancel();
            let unpublished = control.resources.cancel(target);
            drop(control);
            drop(unpublished);
        }
        result
    }
}
