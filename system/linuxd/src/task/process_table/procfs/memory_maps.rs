//! proc maps 的 MM 观察引用；成功打开后不再解析 PID，不保活运行资源。
use super::PROCESS_TABLE;
use crate::fs::{FileSystemError, ProcMapping, ProcMemoryMaps};
use crate::memory::{AnonymousRole, MapPermission, MappingBacking, MemoryError};
use crate::task::model::AddressSpace;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

struct MemoryMaps(Weak<AddressSpace>);

pub(super) fn open(tid: usize) -> Result<Arc<dyn ProcMemoryMaps>, FileSystemError> {
    // 1. graph 只捕获实际 Thread/Process，不持 graph 等待 exec/credential guard。
    let (target, process) = {
        let graph = PROCESS_TABLE.graph.lock();
        (
            super::super::thread_selector::execution_by_tid(&graph, tid),
            graph.nodes.contains_key(&tid),
        )
    };
    let caller = crate::task::current_task().ok_or(FileSystemError::AccessDenied)?;
    let mm = if let Some(target) = target {
        // 2. 与完整 exec 串行授权和 MM 捕获，不能检查旧凭据后绑定新的特权映像。
        let _credentials =
            target
                .credential_guard()
                .lock_killable()
                .map_err(|error| match error {
                    crate::sync::TaskMutexLockError::OutOfMemory => FileSystemError::OutOfMemory,
                    crate::sync::TaskMutexLockError::Interrupted => {
                        FileSystemError::Errno(linux_abi::errno::EINTR as u16)
                    }
                })?;
        if let Some(mm) = target.namespace_mm() {
            let observed = Arc::downgrade(&mm);
            if !caller.may_read_memory_maps(&target, mm) {
                return Err(FileSystemError::AccessDenied);
            }
            observed
        } else {
            Weak::new()
        }
    } else if process {
        Weak::new()
    } else {
        return Err(FileSystemError::NotFound);
    };
    // 3. Weak 对应 mmgrab，读取时 upgrade 对应 mmget_not_zero；最后 user 退出后保持 EOF。
    Arc::try_new(MemoryMaps(mm))
        .map(|maps| maps as Arc<dyn ProcMemoryMaps>)
        .map_err(|_| FileSystemError::OutOfMemory)
}

impl ProcMemoryMaps for MemoryMaps {
    fn snapshot(&self, detailed: bool) -> Result<Vec<ProcMapping>, FileSystemError> {
        let Some(mm) = self.0.upgrade() else {
            return Ok(Vec::new());
        };
        let areas = mm.mapping_snapshot(detailed).map_err(|error| match error {
            MemoryError::OutOfMemory => FileSystemError::OutOfMemory,
            MemoryError::PermissionDenied => FileSystemError::AccessDenied,
            MemoryError::InvalidRange | MemoryError::AddressInUse => {
                FileSystemError::InvalidOperation
            }
            MemoryError::Io => FileSystemError::IoError,
        })?;
        let mut mappings = Vec::new();
        mappings
            .try_reserve_exact(areas.len())
            .map_err(|_| FileSystemError::OutOfMemory)?;
        for area in areas {
            let (file, offset) = match &area.backing {
                MappingBacking::File { mapping, offset } => {
                    (Some(crate::fs::mapping_file(&**mapping)?), *offset)
                }
                MappingBacking::Device {
                    file: Some(file),
                    offset,
                } => {
                    let owner: &dyn core::any::Any = &**file;
                    let file = owner
                        .downcast_ref::<crate::file::OpenFileDescription>()
                        .ok_or(FileSystemError::InvalidOperation)?;
                    (Some(file.opened_ref()), *offset)
                }
                MappingBacking::Device { file: None, offset } => (None, *offset),
                MappingBacking::SharedMemory { file, offset } => {
                    let file: Arc<dyn core::any::Any + Send + Sync> = file.clone();
                    (
                        Some(
                            file.downcast::<crate::fs::OpenedFile>()
                                .map_err(|_| FileSystemError::InvalidOperation)?,
                        ),
                        *offset,
                    )
                }
                MappingBacking::Anonymous(_) => (None, 0),
            };
            let label: Option<&'static [u8]> = match area.backing {
                MappingBacking::Anonymous(AnonymousRole::Heap) => Some(b"[heap]"),
                MappingBacking::Anonymous(AnonymousRole::Stack) => Some(b"[stack]"),
                _ => None,
            };
            mappings.push(ProcMapping {
                start: area.start,
                end: area.end,
                permissions: [MapPermission::R, MapPermission::W, MapPermission::X]
                    .map(|permission| area.permission.contains(permission)),
                shared: area.shared,
                offset,
                file,
                name: area.name,
                label,
                statistics: area.statistics,
            });
        }
        Ok(mappings)
    }
}
