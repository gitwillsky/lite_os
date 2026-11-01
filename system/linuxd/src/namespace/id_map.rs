//! 一次发布后不可变的 Linux UID/GID range map；kernel ID 是全局事实，local ID 仅为 projection。
use super::NamespaceError;
use alloc::vec::Vec;
const MAX_EXTENTS: usize = 340;

/// 一个 namespace-local 连续 range 到 global kuid/kgid range 的映射；MAX ID 从不有效。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MapExtent {
    pub(crate) first: u32,
    pub(crate) lower_first: u32,
    pub(crate) count: u32,
}
impl MapExtent {
    fn end(first: u32, count: u32) -> Option<u32> {
        if count == 0 {
            return None;
        }
        first.checked_add(count)
    }
    fn valid(self) -> bool {
        Self::end(self.first, self.count).is_some()
            && Self::end(self.lower_first, self.count).is_some()
    }
}

/// 写入顺序的 canonical extents 与只含 index 的双向 projection；所有排序/分配先于 publication。
/// proc 保留输入顺序，local/global lookup 均二分；不复制 extent records 或递归 parent-map 热路径。
pub(crate) struct IdMap {
    extents: Vec<MapExtent>,
    local_order: Vec<usize>,
    global_order: Vec<usize>,
}
impl IdMap {
    /// 解析固定 Linux proc uid_map/gid_map 的十进制三列与 range 约束。
    /// 返回的 lower_first 尚为 parent-local ID，caller 必须在 prepare 前转换为全局 ID。
    pub(crate) fn parse(bytes: &[u8]) -> Result<Vec<MapExtent>, NamespaceError> {
        if bytes.is_empty() || bytes.len() >= 4096 || bytes.contains(&0) {
            return Err(NamespaceError::Invalid);
        }
        let text = core::str::from_utf8(bytes).map_err(|_| NamespaceError::Invalid)?;
        let mut extents = Vec::new();
        for line in text.split_terminator('\n') {
            let mut fields = line.split_ascii_whitespace();
            let mut number = || {
                let token = fields.next().ok_or(NamespaceError::Invalid)?;
                if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(NamespaceError::Invalid);
                }
                token.parse::<u32>().map_err(|_| NamespaceError::Invalid)
            };
            let extent = MapExtent {
                first: number()?,
                lower_first: number()?,
                count: number()?,
            };
            if fields.next().is_some() || !extent.valid() || extents.len() == MAX_EXTENTS {
                return Err(NamespaceError::Invalid);
            }
            extents
                .try_reserve(1)
                .map_err(|_| NamespaceError::OutOfMemory)?;
            extents.push(extent);
        }
        Self::check_overlap(&extents)?;
        Ok(extents)
    }
    fn check_overlap(extents: &[MapExtent]) -> Result<(), NamespaceError> {
        if extents.is_empty() || extents.len() > MAX_EXTENTS {
            return Err(NamespaceError::Invalid);
        }
        for (index, extent) in extents.iter().enumerate() {
            if !extent.valid() {
                return Err(NamespaceError::Invalid);
            }
            for other in &extents[..index] {
                let overlap = |a: u32, ac: u32, b: u32, bc: u32| {
                    u64::from(a) < u64::from(b) + u64::from(bc)
                        && u64::from(b) < u64::from(a) + u64::from(ac)
                };
                if overlap(extent.first, extent.count, other.first, other.count)
                    || overlap(
                        extent.lower_first,
                        extent.count,
                        other.lower_first,
                        other.count,
                    )
                {
                    return Err(NamespaceError::Invalid);
                }
            }
        }
        Ok(())
    }
    /// 所有 lower range 已成为 global kernel ID；准备唯一不可变 map 与双向 index。
    pub(crate) fn prepare(extents: Vec<MapExtent>) -> Result<Self, NamespaceError> {
        Self::check_overlap(&extents)?;
        let mut local_order = Vec::new();
        local_order
            .try_reserve_exact(extents.len())
            .map_err(|_| NamespaceError::OutOfMemory)?;
        local_order.extend(0..extents.len());
        local_order.sort_unstable_by_key(|index| extents[*index].first);
        let mut global_order = Vec::new();
        global_order
            .try_reserve_exact(extents.len())
            .map_err(|_| NamespaceError::OutOfMemory)?;
        global_order.extend(0..extents.len());
        global_order.sort_unstable_by_key(|index| extents[*index].lower_first);
        Ok(Self {
            extents,
            local_order,
            global_order,
        })
    }
    pub(crate) fn extents(&self) -> &[MapExtent] {
        &self.extents
    }

    /// Parent-local range 必须完整落在同一连续 extent，不允许端点检查跨越未映射 hole。
    pub(crate) fn range_to_kernel(&self, local: u32, count: u32) -> Option<u32> {
        let end = MapExtent::end(local, count)?;
        let index = self
            .local_order
            .partition_point(|index| self.extents[*index].first <= local)
            .checked_sub(1)?;
        let extent = self.extents[self.local_order[index]];
        if end > extent.first + extent.count {
            return None;
        }
        Some(extent.lower_first + (local - extent.first))
    }
    pub(crate) fn map_kernel_id(&self, kernel: u32) -> Option<u32> {
        let index = self
            .global_order
            .partition_point(|index| self.extents[*index].lower_first <= kernel)
            .checked_sub(1)?;
        let extent = self.extents[self.global_order[index]];
        let delta = kernel - extent.lower_first;
        (delta < extent.count).then(|| extent.first + delta)
    }
}
