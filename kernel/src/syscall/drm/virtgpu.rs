use alloc::vec::Vec;

use crate::{
    drm::{
        DrmFile, DrmSubmission, VIRGL_COMMAND_MAX, VirglBox, VirglCommand, VirglResourceCreate,
        VirglTransferDirection,
    },
    task::TaskControlBlock,
};

use super::{
    copy_in, copy_out, drm_errno, errno, read_u32, read_u64, wait_retry, wait_scanout, write_u32,
    write_u64,
};

const VIRTGPU_PARAM_3D_FEATURES: u64 = 1;
const VIRTGPU_PARAM_CAPSET_QUERY_FIX: u64 = 2;
const VIRTGPU_PARAM_RESOURCE_BLOB: u64 = 3;
const VIRTGPU_PARAM_HOST_VISIBLE: u64 = 4;
const VIRTGPU_PARAM_CROSS_DEVICE: u64 = 5;
const VIRTGPU_PARAM_CONTEXT_INIT: u64 = 6;
const VIRTGPU_PARAM_SUPPORTED_CAPSET_IDS: u64 = 7;
const VIRTGPU_PARAM_EXPLICIT_DEBUG_NAME: u64 = 8;

const VIRTGPU_CONTEXT_PARAM_CAPSET_ID: u64 = 1;
const VIRTGPU_CONTEXT_PARAM_NUM_RINGS: u64 = 2;
const VIRTGPU_CONTEXT_PARAM_POLL_RINGS_MASK: u64 = 3;
const VIRTGPU_CONTEXT_PARAM_DEBUG_NAME: u64 = 4;
const VIRTGPU_WAIT_NOWAIT: u32 = 1;
const MAX_EXEC_RESOURCES: usize = 1024;
const MAX_CONTEXT_PARAMS: usize = 4;

pub(super) fn get_param(
    task: &TaskControlBlock,
    file: &DrmFile,
    argument: usize,
) -> Result<(), isize> {
    let bytes = copy_in::<16>(task, argument)?;
    let value = match read_u64(&bytes, 0)? {
        VIRTGPU_PARAM_3D_FEATURES => u64::from(file.virgl_capset_info().is_ok()),
        VIRTGPU_PARAM_CAPSET_QUERY_FIX => 1,
        VIRTGPU_PARAM_CONTEXT_INIT | VIRTGPU_PARAM_EXPLICIT_DEBUG_NAME => {
            u64::from(file.supports_virgl_context_init())
        }
        VIRTGPU_PARAM_SUPPORTED_CAPSET_IDS => file
            .virgl_capset_info()
            .ok()
            .and_then(|capset| 1u64.checked_shl(capset.id))
            .unwrap_or(0),
        VIRTGPU_PARAM_RESOURCE_BLOB | VIRTGPU_PARAM_HOST_VISIBLE | VIRTGPU_PARAM_CROSS_DEVICE => 0,
        _ => return Err(errno::EINVAL),
    };
    copy_out(
        task,
        usize::try_from(read_u64(&bytes, 8)?).map_err(|_| errno::EFAULT)?,
        &value.to_ne_bytes(),
    )
}

pub(super) fn get_caps(
    task: &TaskControlBlock,
    file: &DrmFile,
    argument: usize,
) -> Result<(), isize> {
    let bytes = copy_in::<24>(task, argument)?;
    let capset = file.virgl_capset_info().map_err(drm_errno)?;
    if read_u32(&bytes, 0)? != capset.id || read_u32(&bytes, 4)? != capset.version {
        return Err(errno::EINVAL);
    }
    let requested = usize::try_from(read_u32(&bytes, 16)?).map_err(|_| errno::EINVAL)?;
    let mut capabilities = Vec::new();
    capabilities
        .try_reserve_exact(capset.size)
        .map_err(|_| errno::ENOMEM)?;
    capabilities.resize(capset.size, 0);
    file.copy_virgl_capset(&mut capabilities)
        .map_err(drm_errno)?;
    let count = requested.min(capabilities.len());
    if count != 0 {
        copy_out(
            task,
            usize::try_from(read_u64(&bytes, 8)?).map_err(|_| errno::EFAULT)?,
            &capabilities[..count],
        )?;
    }
    Ok(())
}

pub(super) fn context_init(
    task: &TaskControlBlock,
    file: &DrmFile,
    argument: usize,
) -> Result<(), isize> {
    let bytes = copy_in::<16>(task, argument)?;
    let count = usize::try_from(read_u32(&bytes, 0)?).map_err(|_| errno::EINVAL)?;
    if count == 0 || count > MAX_CONTEXT_PARAMS || read_u32(&bytes, 4)? != 0 {
        return Err(errno::EINVAL);
    }
    let pointer = usize::try_from(read_u64(&bytes, 8)?).map_err(|_| errno::EFAULT)?;
    let mut capset_id = None;
    let mut name = Vec::new();
    for index in 0..count {
        let address = pointer
            .checked_add(index.checked_mul(16).ok_or(errno::EFAULT)?)
            .ok_or(errno::EFAULT)?;
        let parameter = copy_in::<16>(task, address)?;
        let value = read_u64(&parameter, 8)?;
        match read_u64(&parameter, 0)? {
            VIRTGPU_CONTEXT_PARAM_CAPSET_ID => {
                capset_id = Some(u32::try_from(value).map_err(|_| errno::EINVAL)?);
            }
            VIRTGPU_CONTEXT_PARAM_NUM_RINGS if value == 1 => {}
            VIRTGPU_CONTEXT_PARAM_POLL_RINGS_MASK if value == 0 => {}
            VIRTGPU_CONTEXT_PARAM_DEBUG_NAME => {
                name = task
                    .copy_user_c_string(usize::try_from(value).map_err(|_| errno::EFAULT)?, 65)
                    .map_err(|_| errno::EFAULT)?;
                if name.len() > 64 {
                    return Err(errno::EINVAL);
                }
            }
            _ => return Err(errno::EINVAL),
        }
    }
    let prepared = file
        .prepare_virgl_context(capset_id.ok_or(errno::EINVAL)?, &name)
        .map_err(drm_errno)?;
    submit_and_wait(file, prepared.command())?;
    prepared.publish();
    Ok(())
}

pub(super) fn resource_create(
    task: &TaskControlBlock,
    file: &DrmFile,
    argument: usize,
) -> Result<(), isize> {
    let mut bytes = copy_in::<56>(task, argument)?;
    if read_u32(&bytes, 40)? != 0 {
        return Err(errno::EINVAL);
    }
    if let Some(context) = file.prepare_legacy_virgl_context().map_err(drm_errno)? {
        submit_and_wait(file, context.command())?;
        context.publish();
    }
    let prepared = file
        .prepare_virgl_resource(VirglResourceCreate {
            target: read_u32(&bytes, 0)?,
            format: read_u32(&bytes, 4)?,
            bind: read_u32(&bytes, 8)?,
            width: read_u32(&bytes, 12)?,
            height: read_u32(&bytes, 16)?,
            depth: read_u32(&bytes, 20)?,
            array_size: read_u32(&bytes, 24)?,
            last_level: read_u32(&bytes, 28)?,
            samples: read_u32(&bytes, 32)?,
            flags: read_u32(&bytes, 36)?,
            size: read_u64(&bytes, 48)?,
        })
        .map_err(drm_errno)?;
    submit_and_wait(file, prepared.create_command())?;
    submit_and_wait(file, prepared.attach_command())?;
    submit_and_wait(file, prepared.context_attach_command())?;
    let info = prepared.info();
    write_u32(&mut bytes, 40, info.handle)?;
    write_u32(&mut bytes, 44, info.resource_id)?;
    if let Err(error) = copy_out(task, argument, &bytes) {
        let (context_id, resource_id) = prepared.identities();
        let _ = submit_and_wait(
            file,
            VirglCommand::ContextDetachResource {
                context_id,
                resource_id,
            },
        );
        let _ = submit_and_wait(file, VirglCommand::ResourceDetachBacking { resource_id });
        let _ = submit_and_wait(file, VirglCommand::ResourceUnref { resource_id });
        return Err(error);
    }
    prepared.publish();
    Ok(())
}

pub(super) fn resource_info(
    task: &TaskControlBlock,
    file: &DrmFile,
    argument: usize,
) -> Result<(), isize> {
    let mut bytes = copy_in::<16>(task, argument)?;
    let info = file
        .virgl_resource_info(read_u32(&bytes, 0)?)
        .map_err(drm_errno)?;
    bytes.fill(0);
    write_u32(&mut bytes, 0, info.handle)?;
    write_u32(&mut bytes, 4, info.resource_id)?;
    write_u32(&mut bytes, 8, info.size)?;
    copy_out(task, argument, &bytes)
}

pub(super) fn map(task: &TaskControlBlock, file: &DrmFile, argument: usize) -> Result<(), isize> {
    let mut bytes = copy_in::<16>(task, argument)?;
    let offset = file.map_virgl(read_u32(&bytes, 8)?).map_err(drm_errno)?;
    write_u64(&mut bytes, 0, offset)?;
    copy_out(task, argument, &bytes)
}

pub(super) fn transfer(
    task: &TaskControlBlock,
    file: &DrmFile,
    argument: usize,
    direction: VirglTransferDirection,
) -> Result<(), isize> {
    let bytes = copy_in::<44>(task, argument)?;
    let handle = read_u32(&bytes, 0)?;
    let command = file
        .transfer_command(crate::drm::VirglTransfer {
            handle,
            direction,
            offset: read_u32(&bytes, 32)?,
            level: read_u32(&bytes, 28)?,
            region: VirglBox {
                x: read_u32(&bytes, 4)?,
                y: read_u32(&bytes, 8)?,
                z: read_u32(&bytes, 12)?,
                width: read_u32(&bytes, 16)?,
                height: read_u32(&bytes, 20)?,
                depth: read_u32(&bytes, 24)?,
            },
            stride: read_u32(&bytes, 36)?,
            layer_stride: read_u32(&bytes, 40)?,
        })
        .map_err(drm_errno)?;
    let fence = submit_only(file, command)?;
    file.record_virgl_fence(&[handle], fence).map_err(drm_errno)
}

pub(super) fn execbuffer(
    task: &TaskControlBlock,
    file: &DrmFile,
    argument: usize,
) -> Result<(), isize> {
    let bytes = copy_in::<64>(task, argument)?;
    let flags = read_u32(&bytes, 0)?;
    let size = usize::try_from(read_u32(&bytes, 4)?).map_err(|_| errno::EINVAL)?;
    let count = usize::try_from(read_u32(&bytes, 24)?).map_err(|_| errno::EINVAL)?;
    if flags != 0
        || size == 0
        || size > VIRGL_COMMAND_MAX
        || !size.is_multiple_of(4)
        || count > MAX_EXEC_RESOURCES
        || read_u32(&bytes, 28)? != u32::MAX
        || read_u32(&bytes, 32)? != 0
        || read_u32(&bytes, 36)? != 0
        || read_u32(&bytes, 40)? != 0
        || read_u32(&bytes, 44)? != 0
        || read_u64(&bytes, 48)? != 0
        || read_u64(&bytes, 56)? != 0
    {
        return Err(errno::EINVAL);
    }
    let mut commands = try_zeroed(size)?;
    task.copy_from_user(
        usize::try_from(read_u64(&bytes, 8)?).map_err(|_| errno::EFAULT)?,
        &mut commands,
    )
    .map_err(|_| errno::EFAULT)?;
    let mut handles = try_zeroed(count.checked_mul(4).ok_or(errno::EINVAL)?)?;
    if !handles.is_empty() {
        task.copy_from_user(
            usize::try_from(read_u64(&bytes, 16)?).map_err(|_| errno::EFAULT)?,
            &mut handles,
        )
        .map_err(|_| errno::EFAULT)?;
    }
    let mut decoded = Vec::new();
    decoded
        .try_reserve_exact(count)
        .map_err(|_| errno::ENOMEM)?;
    for bytes in handles.as_chunks::<4>().0 {
        decoded.push(u32::from_ne_bytes(*bytes));
    }
    let context_id = file.validate_exec_resources(&decoded).map_err(drm_errno)?;
    let fence = submit_only(
        file,
        VirglCommand::Submit3d {
            context_id,
            commands: &commands,
        },
    )?;
    file.record_virgl_fence(&decoded, fence).map_err(drm_errno)
}

pub(super) fn wait(task: &TaskControlBlock, file: &DrmFile, argument: usize) -> Result<(), isize> {
    let bytes = copy_in::<8>(task, argument)?;
    let flags = read_u32(&bytes, 4)?;
    if flags & !VIRTGPU_WAIT_NOWAIT != 0 {
        return Err(errno::EINVAL);
    }
    let Some(wait) = file.virgl_wait(read_u32(&bytes, 0)?).map_err(drm_errno)? else {
        return Ok(());
    };
    if flags == VIRTGPU_WAIT_NOWAIT {
        return if wait.prepare_to_block().is_none() {
            Ok(())
        } else {
            Err(errno::EBUSY)
        };
    }
    wait_scanout(wait)
}

pub(super) fn gem_close(
    task: &TaskControlBlock,
    file: &DrmFile,
    argument: usize,
) -> Result<(), isize> {
    let bytes = copy_in::<8>(task, argument)?;
    if read_u32(&bytes, 4)? != 0 {
        return Err(errno::EINVAL);
    }
    let prepared = file
        .prepare_virgl_close(read_u32(&bytes, 0)?)
        .map_err(drm_errno)?;
    if let Some(wait) = prepared.wait() {
        wait_scanout(wait)?;
    }
    submit_and_wait(file, prepared.unref_command())?;
    prepared.publish();
    Ok(())
}

fn submit_and_wait(file: &DrmFile, command: VirglCommand<'_>) -> Result<u64, isize> {
    loop {
        match file.submit_virgl(command.clone()).map_err(drm_errno)? {
            DrmSubmission::Wait(wait) => {
                let fence = wait.fence();
                wait_scanout(wait)?;
                return Ok(fence);
            }
            DrmSubmission::Retry(retry) => wait_retry(retry)?,
        }
    }
}

fn submit_only(file: &DrmFile, command: VirglCommand<'_>) -> Result<u64, isize> {
    loop {
        match file.submit_virgl(command.clone()).map_err(drm_errno)? {
            DrmSubmission::Wait(wait) => return Ok(wait.fence()),
            DrmSubmission::Retry(retry) => wait_retry(retry)?,
        }
    }
}

fn try_zeroed(length: usize) -> Result<Vec<u8>, isize> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(length).map_err(|_| errno::ENOMEM)?;
    bytes.resize(length, 0);
    Ok(bytes)
}
