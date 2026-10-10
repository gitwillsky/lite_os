//! 一块物理盘上全部 bdev extent 的唯一 claim owner，覆盖整盘/分区别名。

use super::FileSystemError;
use crate::fallible_tree::FallibleMap;
use alloc::sync::Arc;
use core::ops::Range;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClaimState {
    Free,
    Writers(usize),
    Mounted,
    Swap,
}

struct Extent {
    bytes: Range<u64>,
    state: ClaimState,
}

struct DiskClaims {
    // OWNER: physical extent、mount 与 writer 同锁提交；guard 内不分配、不 I/O、
    // 不调用 page-cache/scheduler。所有 caller 为 task context，Drop 只做有界树 mutation。
    extents: spin::Mutex<FallibleMap<u64, Extent>>,
}

/// bdev 的稳定 extent claim；持有唯一盘 owner 与节点 identity，不复制 claim state。
pub(super) struct BlockClaim {
    disk: Arc<DiskClaims>,
    key: u64,
}

fn overlaps(left: &Range<u64>, right: &Range<u64>) -> bool {
    left.start < right.end && right.start < left.end
}

impl BlockClaim {
    /// 创建整盘 claim；metadata OOM 在发布任何 bdev 前返回。
    pub(super) fn whole(key: u64, bytes: u64) -> Result<Self, FileSystemError> {
        if bytes == 0 {
            return Err(FileSystemError::InvalidOperation);
        }
        let mut extents = FallibleMap::new();
        extents
            .try_insert(
                key,
                Extent {
                    bytes: 0..bytes,
                    state: ClaimState::Free,
                },
            )
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let disk = Arc::try_new(DiskClaims {
            extents: spin::Mutex::new(extents),
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        Ok(Self { disk, key })
    }

    /// 在同一物理 owner 中登记分区；相对父 extent 的范围必须完整落在父内。
    pub(super) fn partition(
        &self,
        key: u64,
        start: u64,
        bytes: u64,
    ) -> Result<Self, FileSystemError> {
        if bytes == 0 {
            return Err(FileSystemError::InvalidOperation);
        }
        let range = {
            let extents = self.disk.extents.lock();
            let parent = &extents
                .get(&self.key)
                .expect("bdev claim lost extent")
                .bytes;
            let first = parent
                .start
                .checked_add(start)
                .ok_or(FileSystemError::InvalidOperation)?;
            let end = first
                .checked_add(bytes)
                .filter(|end| *end <= parent.end)
                .ok_or(FileSystemError::InvalidOperation)?;
            first..end
        };
        // allocation 可重入回收/Drop，因此必须在 claim lock 外准备节点。
        let prepared = FallibleMap::try_prepare(
            key,
            Extent {
                bytes: range,
                state: ClaimState::Free,
            },
        )
        .map_err(|_| FileSystemError::OutOfMemory)?;
        let mut extents = self.disk.extents.lock();
        if extents.contains_key(&key) {
            return Err(FileSystemError::AlreadyExists);
        }
        extents.commit_vacant(prepared);
        Ok(Self {
            disk: self.disk.clone(),
            key,
        })
    }

    /// 登记 raw writer；同一 bdev 可有多个 OFD，重叠的其他 bdev 不可同时写或挂载。
    pub(super) fn begin_writer(&self) -> Result<(), FileSystemError> {
        let mut extents = self.disk.extents.lock();
        let own = extents.get(&self.key).expect("bdev claim lost extent");
        if extents.iter().any(|(&key, extent)| {
            overlaps(&own.bytes, &extent.bytes)
                && (matches!(extent.state, ClaimState::Mounted | ClaimState::Swap)
                    || key != self.key && extent.state != ClaimState::Free)
        }) {
            return Err(FileSystemError::Busy);
        }
        let own = extents.get_mut(&self.key).expect("bdev claim lost extent");
        let count = match own.state {
            ClaimState::Free => 0,
            ClaimState::Writers(count) => count,
            ClaimState::Mounted | ClaimState::Swap => unreachable!(),
        };
        own.state = ClaimState::Writers(count.checked_add(1).ok_or(FileSystemError::Busy)?);
        Ok(())
    }

    /// 撤销一个 raw writer；OFD Drop 不分配、不 I/O，也不等待 scheduler。
    pub(super) fn end_writer(&self) {
        let mut extents = self.disk.extents.lock();
        let own = extents.get_mut(&self.key).expect("bdev claim lost extent");
        let ClaimState::Writers(count) = own.state else {
            panic!("block writer released without acquire");
        };
        own.state = if count == 1 {
            ClaimState::Free
        } else {
            ClaimState::Writers(count - 1)
        };
    }

    /// 在任何 storage/cache I/O 前排他地领取 extent，阻止重叠别名新增 writer/mount。
    pub(super) fn begin_mount(&self) -> Result<(), FileSystemError> {
        self.begin_exclusive(ClaimState::Mounted)
    }

    pub(super) fn begin_swap(&self) -> Result<(), FileSystemError> {
        self.begin_exclusive(ClaimState::Swap)
    }

    fn begin_exclusive(&self, state: ClaimState) -> Result<(), FileSystemError> {
        let mut extents = self.disk.extents.lock();
        let own = extents.get(&self.key).expect("bdev claim lost extent");
        if extents
            .values()
            .any(|extent| overlaps(&own.bytes, &extent.bytes) && extent.state != ClaimState::Free)
        {
            return Err(FileSystemError::Busy);
        }
        extents
            .get_mut(&self.key)
            .expect("bdev claim lost extent")
            .state = state;
        Ok(())
    }

    /// 完成卸载或撤销失败的挂载准备。
    pub(super) fn end_mount(&self) {
        self.end_exclusive(ClaimState::Mounted);
    }

    pub(super) fn end_swap(&self) {
        self.end_exclusive(ClaimState::Swap);
    }

    fn end_exclusive(&self, expected: ClaimState) {
        let mut extents = self.disk.extents.lock();
        let own = extents.get_mut(&self.key).expect("bdev claim lost extent");
        assert!(
            own.state == expected,
            "block mount released without acquire"
        );
        own.state = ClaimState::Free;
    }

    /// 当前 bdev 是否必须绕过私有 cache；别名 writer 的领取同时阻止新 alias cache。
    /// 自身 writer 仍使用唯一自有 cache，Snapshot 只读取已写回的块层内容。
    pub(super) fn bypass_cache(&self) -> bool {
        let extents = self.disk.extents.lock();
        let own = extents.get(&self.key).expect("bdev claim lost extent");
        extents.iter().any(|(&key, extent)| {
            overlaps(&own.bytes, &extent.bytes)
                && (matches!(extent.state, ClaimState::Mounted | ClaimState::Swap)
                    || key != self.key && matches!(extent.state, ClaimState::Writers(_)))
        })
    }

    /// 返回 after 之后的下一个重叠 alias key，供锁外逐一写回/逐出 cache；不分配快照。
    pub(super) fn next_alias(&self, after: Option<u64>) -> Option<u64> {
        let extents = self.disk.extents.lock();
        let own = extents.get(&self.key).expect("bdev claim lost extent");
        match after {
            Some(key) => extents
                .iter_after(&key)
                .find(|(_, extent)| overlaps(&own.bytes, &extent.bytes)),
            None => extents
                .iter()
                .find(|(_, extent)| overlaps(&own.bytes, &extent.bytes)),
        }
        .map(|(&key, _)| key)
    }
}

impl Drop for BlockClaim {
    fn drop(&mut self) {
        let mut extents = self.disk.extents.lock();
        let extent = extents
            .remove(&self.key)
            .expect("bdev claim extent removed twice");
        assert!(
            extent.state == ClaimState::Free,
            "active bdev claim lost its lifetime owner"
        );
    }
}
