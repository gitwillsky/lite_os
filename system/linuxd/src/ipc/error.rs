/// System V IPC 领域失败；syscall seam 统一映射标准 errno。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IpcError {
    Invalid,
    Permission,
    Access,
    NoMemory,
    NoSpace,
    Exists,
    NotFound,
    Removed,
    TooBig,
    Again,
    Range,
    Index,
}
