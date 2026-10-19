//! io_uring（Linux v7.1 子集）：提交队列/完成队列共享内存 ring 与 NT 完成端口式的完成模型。
//!
//! 本 module 拥有 ring 的内存布局、SQE 校验与 CQE 发布；`syscall` 只做参数解码、user-copy 与 errno，
//! ring fd 的创建和 mmap 经 `fs::chardev::DeviceFile` 注册的文件操作完成。支持范围与明确拒绝的
//! 能力见 `docs/syscall-support/` 与 `docs/architecture-contract/io-uring.md`。

pub(crate) mod abi;
mod file;
mod ops;
mod ring;

use alloc::sync::Arc;

pub(crate) use ops::{Submitter, fail_init};
pub(crate) use ring::{CqWait, Flow, Ring, RingError};

use abi::{Geometry, PARAMS_BYTES, SetupParams};

/// ring fd 在 `/proc/<pid>/fd` 中的匿名 inode 标签。
pub(crate) const ANONYMOUS_NAME: &[u8] = b"anon_inode:[io_uring]";

/// `io_uring_setup` 的领域失败；syscall 层映射为 `EINVAL`/`ENOMEM`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SetupFailure {
    Invalid,
    OutOfMemory,
}

/// 校验 `entries` 与用户 `io_uring_params`，创建 ring 并编码返回给用户的 params。
///
/// # Errors
///
/// 参数非法返回 `Invalid`；分配失败返回 `OutOfMemory`，不留下部分状态。
pub(crate) fn create(
    entries: u32,
    params: &[u8; PARAMS_BYTES],
) -> Result<(Arc<Ring>, [u8; PARAMS_BYTES]), SetupFailure> {
    let params = SetupParams::decode(params).map_err(|_| SetupFailure::Invalid)?;
    let geometry = Geometry::new(entries, params).map_err(|_| SetupFailure::Invalid)?;
    let ring = Ring::try_new(geometry).map_err(|_| SetupFailure::OutOfMemory)?;
    Ok((ring, geometry.encode_params(params.flags)))
}

/// 消费并执行至多 `to_submit` 个 SQE，返回被提交的条目数。
///
/// # Errors
///
/// 等待 `submit_lock` 的元数据分配失败返回 `OutOfMemory`。
pub(crate) fn submit(
    ring: &Arc<Ring>,
    to_submit: u32,
    submitter: &dyn Submitter,
) -> Result<u32, RingError> {
    ring.submit(to_submit, |sqe| ops::issue(ring, sqe, submitter))
}

/// 若 `file` 是 io_uring ring，取回它的所有权句柄；挂起请求需要在提交返回后继续持有它。
///
/// 经 `Any` 向下转型而不是在 `DeviceFile` 上加访问器：访问器会让 `fs` 反向依赖 `io_uring`，
/// 而后者本来就依赖 `fs::chardev`，形成模块环。
pub(crate) fn ring_arc_of(file: &Arc<dyn crate::fs::chardev::DeviceFile>) -> Option<Arc<Ring>> {
    let any: Arc<dyn core::any::Any + Send + Sync> = file.clone();
    any.downcast::<Ring>().ok()
}
