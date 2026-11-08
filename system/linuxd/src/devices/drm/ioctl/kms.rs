//! KMS plane/property 查询 ABI；运行值从 DRM owner 读取，不复制活动状态。
use super::super::kms::{Object, PLANE_TYPES, PRIMARY_PLANE_ID, PRIMARY_PLANE_TYPE, Property};
use super::*;

pub(super) fn set_client_cap(
    user: &dyn UserMemory,
    file: &DrmFile,
    argument: usize,
) -> Result<(), isize> {
    let request = copy_in::<16>(user, argument)?;
    match read_u64(&request, 0)? {
        2 => {
            let value = read_u64(&request, 8)?;
            if value > 1 {
                return Err(errno::EINVAL);
            }
            file.set_universal_planes(value != 0);
            Ok(())
        }
        3 => {
            let value = read_u64(&request, 8)?;
            if value > 1 {
                return Err(errno::EINVAL);
            }
            file.set_atomic(value != 0);
            Ok(())
        }
        _ => Err(errno::EINVAL),
    }
}
pub(super) fn plane_resources(
    user: &dyn UserMemory,
    file: &DrmFile,
    argument: usize,
) -> Result<(), isize> {
    let mut request = copy_in::<16>(user, argument)?;
    let count = u32::from(file.universal_planes());
    if count != 0 {
        copy_id_array(
            user,
            read_u64(&request, 0)?,
            read_u32(&request, 8)?,
            PRIMARY_PLANE_ID,
        )?;
    }
    write_u32(&mut request, 8, count)?;
    copy_out(user, argument, &request)
}
pub(super) fn plane(user: &dyn UserMemory, file: &DrmFile, argument: usize) -> Result<(), isize> {
    let mut request = copy_in::<32>(user, argument)?;
    if read_u32(&request, 0)? != PRIMARY_PLANE_ID {
        return Err(errno::ENOENT);
    }
    let binding = file
        .device
        .completion
        .lock()
        .configuration()
        .map_or((0, 0), |configuration| configuration.primary_binding());
    write_u32(&mut request, 4, binding.0)?;
    write_u32(&mut request, 8, binding.1)?;
    write_u32(&mut request, 12, 1)?; // 只有逻辑 CRTC index 0；不是 object ID 的位图。
    write_u32(&mut request, 16, 0)?;
    copy_id_array(
        user,
        read_u64(&request, 24)?,
        read_u32(&request, 20)?,
        DRM_FORMAT_XRGB8888,
    )?;
    write_u32(&mut request, 20, 1)?;
    copy_out(user, argument, &request)
}
pub(super) fn object_properties(
    user: &dyn UserMemory,
    file: &DrmFile,
    argument: usize,
) -> Result<(), isize> {
    let mut request = copy_in::<32>(user, argument)?;
    let id = read_u32(&request, 20)?;
    let kind = read_u32(&request, 24)?;
    let object = Object::find(id, kind).ok_or_else(|| {
        if file.mode_object_without_properties(id, kind) {
            errno::EINVAL
        } else {
            errno::ENOENT
        }
    })?;
    // 只在 OFD 协商 atomic 后公布可写属性；不可变 type 对 legacy 客户端仍可见。
    let atomic = file.atomic();
    let mut snapshot = [(0u32, 0u64); 14];
    let mut count = 0usize;
    {
        let completion = file.device.completion.lock();
        for &property in object.properties() {
            if !atomic && property.flags() & (1 << 31) != 0 {
                continue;
            }
            let value = completion.configuration().map_or_else(
                || {
                    if property == Property::PlaneType {
                        PRIMARY_PLANE_TYPE
                    } else {
                        0
                    }
                },
                |configuration| configuration.value(object, property),
            );
            snapshot[count] = (property as u32, value);
            count += 1;
        }
    }
    let capacity = read_u32(&request, 16)? as usize;
    let ids = usize::try_from(read_u64(&request, 0)?).map_err(|_| errno::EFAULT)?;
    let values = usize::try_from(read_u64(&request, 8)?).map_err(|_| errno::EFAULT)?;
    for (index, (id, value)) in snapshot[..count].iter().take(capacity).enumerate() {
        copy_out(
            user,
            ids.checked_add(index * 4).ok_or(errno::EFAULT)?,
            &id.to_ne_bytes(),
        )?;
        copy_out(
            user,
            values.checked_add(index * 8).ok_or(errno::EFAULT)?,
            &value.to_ne_bytes(),
        )?;
    }
    write_u32(&mut request, 16, count as u32)?;
    copy_out(user, argument, &request)
}
pub(super) fn property(user: &dyn UserMemory, argument: usize) -> Result<(), isize> {
    let mut request = copy_in::<64>(user, argument)?;
    let property = Property::find(read_u32(&request, 16)?).ok_or(errno::ENOENT)?;
    let values = property.values();
    let capacity = read_u32(&request, 56)? as usize;
    let address = usize::try_from(read_u64(&request, 0)?).map_err(|_| errno::EFAULT)?;
    for (index, value) in values.iter().take(capacity).enumerate() {
        copy_out(
            user,
            address.checked_add(index * 8).ok_or(errno::EFAULT)?,
            &value.to_ne_bytes(),
        )?;
    }
    write_u32(&mut request, 20, property.flags())?;
    request[24..56].fill(0);
    request[24..24 + property.name().len()].copy_from_slice(property.name());
    write_u32(&mut request, 56, values.len() as u32)?;
    let enumeration = if property == Property::PlaneType {
        &PLANE_TYPES[..]
    } else {
        &[]
    };
    let capacity = read_u32(&request, 60)? as usize;
    let address = usize::try_from(read_u64(&request, 8)?).map_err(|_| errno::EFAULT)?;
    for (index, (value, name)) in enumeration.iter().take(capacity).enumerate() {
        let mut entry = [0u8; 40];
        write_u64(&mut entry, 0, *value)?;
        entry[8..8 + name.len()].copy_from_slice(name);
        copy_out(
            user,
            address
                .checked_add(index * entry.len())
                .ok_or(errno::EFAULT)?,
            &entry,
        )?;
    }
    write_u32(&mut request, 60, enumeration.len() as u32)?;
    copy_out(user, argument, &request)
}
