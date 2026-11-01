//! 内核无法解决的用户缺页的分类。
//!
//! 内核先按 VMAR 解决缺页（按需提交、写时复制、pager 补页）；只有映射缺失、权限不符或后备无法提供页时，
//! Linux 线程才以 EXCEPTION 原因退出 restricted mode，由这里决定扩展 `MAP_GROWSDOWN` 区或投递信号。

use super::MemorySet;
use super::area::{AnonymousRole, Area, AreaFlags, Backing};
use crate::memory::{MapPermission, MemoryError, MemoryObject, PAGE_SIZE};

/// 缺页访问类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageFaultAccess {
    Read,
    Write,
    Execute,
}

impl PageFaultAccess {
    fn permission(self) -> MapPermission {
        match self {
            Self::Read => MapPermission::R,
            Self::Write => MapPermission::W,
            Self::Execute => MapPermission::X,
        }
    }
}

/// 段错误原因（`si_code`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SegmentationCause {
    /// SEGV_MAPERR。
    Unmapped,
    /// SEGV_ACCERR。
    AccessDenied,
}

/// 缺页分类结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageFaultOutcome {
    /// 已扩展映射，重新执行即可。
    Handled,
    SegmentationFault(SegmentationCause),
    /// 映射存在但后备无法提供页（越过文件尾、I/O 失败）：SIGBUS。
    BusError,
}

/// `MAP_GROWSDOWN` 区与其下方映射之间保留的间隙（Linux `stack_guard_gap` 默认 256 页）。
const STACK_GUARD_GAP: usize = 256 * PAGE_SIZE;

impl MemorySet {
    /// 分类一次内核无法解决的缺页。
    ///
    /// # Errors
    ///
    /// 扩展栈时内存不足。
    pub(crate) fn handle_page_fault(
        &mut self,
        address: usize,
        access: PageFaultAccess,
    ) -> Result<PageFaultOutcome, MemoryError> {
        if let Some(area) = self.area_at(address) {
            return Ok(if area.permission.contains(access.permission()) {
                PageFaultOutcome::BusError
            } else {
                PageFaultOutcome::SegmentationFault(SegmentationCause::AccessDenied)
            });
        }
        let page = address & !(PAGE_SIZE - 1);
        let Some(above) = self
            .areas
            .iter_after(&address)
            .next()
            .map(|(_, area)| area.clone())
            .filter(|area| area.flags.contains(AreaFlags::GROWS_DOWN))
        else {
            return Ok(PageFaultOutcome::SegmentationFault(
                SegmentationCause::Unmapped,
            ));
        };
        let below_end = self
            .areas
            .floor(&address)
            .map_or(self.base, |(_, area)| area.end);
        if page < below_end.saturating_add(STACK_GUARD_GAP) || page < self.base.max(PAGE_SIZE) {
            return Ok(PageFaultOutcome::SegmentationFault(
                SegmentationCause::Unmapped,
            ));
        }
        if !above.permission.contains(access.permission()) {
            return Ok(PageFaultOutcome::SegmentationFault(
                SegmentationCause::AccessDenied,
            ));
        }
        // 新扩展部分接管 GROWS_DOWN，原最低区成为普通栈区。
        self.insert(Area {
            start: page,
            end: above.start,
            permission: above.permission,
            backing: Backing::Anonymous {
                object: MemoryObject::anonymous(above.start - page, true)?,
                offset: 0,
                role: AnonymousRole::Stack,
            },
            flags: above.flags,
            name: None,
        })?;
        self.areas
            .get_mut(&above.start)
            .expect("growsdown area found above")
            .flags
            .remove(AreaFlags::GROWS_DOWN);
        Ok(PageFaultOutcome::Handled)
    }
}
