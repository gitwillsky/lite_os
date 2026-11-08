//! 标准 drm_mode_atomic LP64 marshal；不在 completion 锁内访问用户地址或分配引用。
use super::*;
use crate::devices::drm::kms::{Changes, Object, Property};

pub(super) fn commit(
    user: &dyn UserMemory,
    file: &Arc<DrmFile>,
    argument: usize,
) -> Result<(), isize> {
    const EVENT: u32 = 1;
    const TEST: u32 = 0x100;
    const NONBLOCK: u32 = 0x200;
    const MODESET: u32 = 0x400;
    if !file.atomic() {
        return Err(errno::EINVAL);
    }
    if !file.is_master() {
        return Err(errno::EACCES);
    }
    let request = copy_in::<56>(user, argument)?;
    let flags = read_u32(&request, 0)?;
    if flags & !(EVENT | TEST | NONBLOCK | MODESET) != 0
        || read_u64(&request, 40)? != 0
        || flags & (TEST | EVENT) == TEST | EVENT
    {
        return Err(errno::EINVAL);
    }
    let mut changes = Changes::new();
    let mut property_index = 0usize;
    for index in 0..read_u32(&request, 4)? as usize {
        let id = read_u32(
            &copy_in::<4>(user, at(read_u64(&request, 8)?, index, 4)?)?,
            0,
        )?;
        let object = Object::find(id, 0).ok_or(errno::ENOENT)?;
        let count = read_u32(
            &copy_in::<4>(user, at(read_u64(&request, 16)?, index, 4)?)?,
            0,
        )?;
        for _ in 0..count {
            let id = read_u32(
                &copy_in::<4>(user, at(read_u64(&request, 24)?, property_index, 4)?)?,
                0,
            )?;
            let property = Property::find(id)
                .filter(|property| object.properties().contains(property))
                .ok_or(errno::ENOENT)?;
            let value = read_u64(
                &copy_in::<8>(user, at(read_u64(&request, 32)?, property_index, 8)?)?,
                0,
            )?;
            changes
                .set(file, object, property, value)
                .map_err(drm_errno)?;
            property_index = property_index.checked_add(1).ok_or(errno::EFAULT)?;
        }
    }
    let event = (flags & EVENT != 0)
        .then(|| read_u64(&request, 48))
        .transpose()?;
    loop {
        match file
            .atomic_commit(&changes, flags & TEST != 0, flags & MODESET != 0, event)
            .map_err(drm_errno)?
        {
            None => return Ok(()),
            Some(DrmSubmission::Wait(wait)) => {
                return if flags & NONBLOCK != 0 {
                    Ok(())
                } else {
                    wait_scanout(wait)
                };
            }
            Some(DrmSubmission::Retry(_)) if flags & NONBLOCK != 0 => return Err(errno::EBUSY),
            Some(DrmSubmission::Retry(retry)) => wait_retry(retry)?,
        }
    }
}
fn at(pointer: u64, index: usize, size: usize) -> Result<usize, isize> {
    usize::try_from(pointer)
        .ok()
        .and_then(|pointer| {
            index
                .checked_mul(size)
                .and_then(|offset| pointer.checked_add(offset))
        })
        .ok_or(errno::EFAULT)
}
