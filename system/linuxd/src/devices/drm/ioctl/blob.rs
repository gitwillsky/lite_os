//! 标准 KMS property blob ioctl；只负责 ABI、user-copy 与 errno。
use super::*;
use alloc::vec::Vec;

pub(super) fn create(user: &dyn UserMemory, file: &DrmFile, argument: usize) -> Result<(), isize> {
    let mut request = copy_in::<16>(user, argument)?;
    let length = read_u32(&request, 8)? as usize;
    // 固定 Linux 的 blob 分配也受 INT_MAX 减对象开销约束；先验证，避免非法长度先触发大分配。
    if !super::super::property::Blob::valid_length(length) {
        return Err(errno::EINVAL);
    }
    let pointer = usize::try_from(read_u64(&request, 0)?).map_err(|_| errno::EFAULT)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(length).map_err(|_| errno::ENOMEM)?;
    bytes.resize(length, 0);
    user.read(pointer, &mut bytes).map_err(|_| errno::EFAULT)?;
    file.prepare_property_blob(bytes)
        .map_err(drm_errno)?
        .complete(|id| {
            write_u32(&mut request, 12, id)?;
            copy_out(user, argument, &request)
        })
}
pub(super) fn get(user: &dyn UserMemory, file: &DrmFile, argument: usize) -> Result<(), isize> {
    let mut request = copy_in::<16>(user, argument)?;
    let blob = file
        .property_blob(read_u32(&request, 0)?)
        .map_err(drm_errno)?;
    // 与 Linux GETPROPBLOB 相同：只有长度精确匹配才复制，不截断为调用方容量。
    if read_u32(&request, 4)? as usize == blob.bytes.len() {
        let address = usize::try_from(read_u64(&request, 8)?).map_err(|_| errno::EFAULT)?;
        copy_out(user, address, &blob.bytes)?;
    }
    write_u32(&mut request, 4, blob.bytes.len() as u32)?;
    copy_out(user, argument, &request)
}
pub(super) fn destroy(user: &dyn UserMemory, file: &DrmFile, argument: usize) -> Result<(), isize> {
    let request = copy_in::<4>(user, argument)?;
    file.destroy_property_blob(read_u32(&request, 0)?)
        .map_err(drm_errno)
}
