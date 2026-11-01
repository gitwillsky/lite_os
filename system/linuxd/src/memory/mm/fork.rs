//! fork：子地址空间是共享进程的新成员；私有后备取快照，共享后备映射同一对象。

use alloc::collections::BTreeMap;
use alloc::sync::Arc;

use super::area::{Area, AreaFlags, Backing};
use super::{Heap, MemorySet};
use crate::memory::{MemoryError, MemoryObject};

impl MemorySet {
    /// 为 fork 复制地址空间。
    ///
    /// 私有后备以整个对象为单位取快照：同一对象拆出的多个 VMA（mprotect 切分、brk 区）在子空间中
    /// 继续共享同一份快照，偏移保持不变，因此 brk 之后仍能调整唯一的 heap 对象。
    ///
    /// # Errors
    ///
    /// 成员进程或快照创建失败；父空间不受影响。
    pub(crate) fn try_clone_for_fork(&self, name: &[u8]) -> Result<Self, MemoryError> {
        let mut child = Self::try_new(name)?;
        // 父对象编号 → 子空间中的对应对象。
        let mut snapshots: BTreeMap<u64, Arc<MemoryObject>> = BTreeMap::new();
        let mut snapshot = |object: &Arc<MemoryObject>| -> Result<Arc<MemoryObject>, MemoryError> {
            if let Some(copy) = snapshots.get(&object.koid()) {
                return Ok(copy.clone());
            }
            let copy = object.snapshot(0, object.size()?)?;
            snapshots.insert(object.koid(), copy.clone());
            Ok(copy)
        };
        for (_, area) in self.areas.iter() {
            if area.flags.contains(AreaFlags::DONT_FORK)
                || matches!(area.backing, Backing::Trampoline { .. })
            {
                continue;
            }
            let backing = match &area.backing {
                Backing::Anonymous {
                    object,
                    offset,
                    role,
                } => {
                    if area.flags.contains(AreaFlags::WIPE_ON_FORK) {
                        Backing::Anonymous {
                            object: MemoryObject::anonymous(area.len(), true)?,
                            offset: 0,
                            role: *role,
                        }
                    } else {
                        Backing::Anonymous {
                            object: snapshot(object)?,
                            offset: *offset,
                            role: *role,
                        }
                    }
                }
                Backing::PrivateFile {
                    source,
                    object,
                    offset,
                    file_offset,
                } => Backing::PrivateFile {
                    source: source.clone(),
                    object: snapshot(object)?,
                    offset: *offset,
                    file_offset: *file_offset,
                },
                shared => shared.clone(),
            };
            child.insert(Area {
                start: area.start,
                end: area.end,
                permission: area.permission,
                backing,
                flags: area.flags,
                name: area.name.clone(),
            })?;
        }
        child.executable_owner = self.executable_owner.clone();
        child.interpreter_owner = self.interpreter_owner.clone();
        child.heap = match &self.heap {
            Some(heap) => Some(Heap {
                start: heap.start,
                brk: heap.brk,
                mapped: heap.mapped,
                object: snapshot(&heap.object)?,
            }),
            None => None,
        };
        child.stack = self.stack.clone();
        child.mmap_top = self.mmap_top;
        child.arguments = self.arguments.clone();
        Ok(child)
    }
}

impl Backing {}
