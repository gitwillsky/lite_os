//! Actual SysV attachment/lock owner; native VMO allocation is an explicit host boundary.
use alloc::sync::Arc;
#[path = "../../../system/linuxd/src/memory/shared_file.rs"]
mod shared_file;
#[path = "../../../system/linuxd/src/memory/shared_memory.rs"]
mod shared_memory;
pub(crate) use shared_file::MappedFileOwner;
pub(crate) use shared_memory::*;
#[derive(Debug)]
pub(crate) enum MemoryError {
    OutOfMemory,
}
pub(crate) const PAGE_SIZE: usize = 4096;
/// Metadata-only VMO seam: these registry tests never fault or access segment bytes.
#[derive(Debug)]
pub(crate) struct MemoryObject;
impl MemoryObject {
    pub(crate) fn anonymous(_: usize, _: bool) -> Result<Arc<Self>, MemoryError> {
        Arc::try_new(Self).map_err(|_| MemoryError::OutOfMemory)
    }
    pub(crate) fn committed_bytes(&self) -> Result<u64, MemoryError> {
        Ok(0)
    }
}

#[derive(Debug)]
struct FileIdentity;
impl MappedFileOwner for FileIdentity {}

pub(crate) fn allocate_shared_memory(
    pages: usize,
) -> Result<Arc<SharedMemoryBacking>, crate::ipc::IpcError> {
    let object = MemoryObject::anonymous(pages * PAGE_SIZE, false)
        .map_err(|_| crate::ipc::IpcError::NoMemory)?;
    let file = Arc::try_new(FileIdentity).map_err(|_| crate::ipc::IpcError::NoMemory)?;
    SharedMemoryBacking::new(object, file).map_err(|_| crate::ipc::IpcError::NoMemory)
}

#[path = "../../../system/linuxd/src/memory/mapping_statistics.rs"]
mod mapping_statistics;
pub(crate) type MappingStatistics = mapping_statistics::MappingStatistics;
