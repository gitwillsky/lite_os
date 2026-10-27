//! PRIME UAPI transaction：fd/handle 在 copyout 前不可见；同一个 dma-buf 在同一 OFD 只对应一个 handle。
use super::{copy_in, copy_out, drm_errno, read_u32, wait_scanout, write_u32};
use crate::{
    drm::{DrmFile, graphics::VirglAttachment},
    fallible_tree::FallibleMap,
    fs::chardev::UserMemory,
};
use alloc::sync::Arc;
use syscall_abi::errno;

pub(super) fn handle_to_fd(
    user: &dyn UserMemory,
    file: &Arc<DrmFile>,
    argument: usize,
) -> Result<(), isize> {
    let bytes = copy_in::<12>(user, argument)?;
    let flags = read_u32(&bytes, 4)?;
    if flags & !(crate::fs::O_CLOEXEC | 2) != 0 {
        return Err(errno::EINVAL);
    }
    let buffer = file.export_prime(read_u32(&bytes, 0)?).map_err(drm_errno)?;
    user.install_device(buffer, flags, argument.checked_add(8).ok_or(errno::EFAULT)?)
        .map_err(|error| error.errno())?;
    Ok(())
}

pub(super) fn fd_to_handle(
    user: &dyn UserMemory,
    file: &Arc<DrmFile>,
    argument: usize,
) -> Result<(), isize> {
    let mut bytes = copy_in::<12>(user, argument)?;
    let device = user
        .device_file(read_u32(&bytes, 8)? as i32)
        .map_err(|error| error.errno())?;
    let buffer = (device.as_ref() as &dyn core::any::Any)
        .downcast_ref::<crate::drm::prime::DmaBuf>()
        .ok_or(errno::EINVAL)?;
    if !Arc::ptr_eq(&buffer.device, &file.device) {
        return Err(errno::EINVAL);
    }
    if let Some(context) = file.prepare_legacy_virgl_context().map_err(drm_errno)? {
        super::virtgpu::submit_only(file, context.command())?;
        context.publish();
    }
    // 同一 OFD 的创建/导入/关闭 admission 共用可睡眠锁，不能在 host attach 或 copyout 时持有 spin guard。
    let _admission = file.context_creation.lock().map_err(|_| errno::ENOMEM)?;
    let state = file.state.lock();
    if let Some((&handle, _)) = state
        .graphics_buffers
        .iter()
        .find(|(_, entry)| Arc::ptr_eq(&entry.buffer, buffer.buffer()))
    {
        drop(state);
        write_u32(&mut bytes, 0, handle)?;
        return copy_out(user, argument, &bytes);
    }
    let context_id = state.context.as_ref().ok_or(errno::EINVAL)?.id;
    drop(state);
    // 旧 handle 已关闭但 detach 尚在途：等 exact completion 后才重建同一 context/resource 关联。
    loop {
        file.progress_retirements();
        let generation = file.device.completion.lock().adapter_generation;
        let pending = file
            .device
            .state
            .lock()
            .graphics_detachments
            .get(&(context_id, buffer.buffer().resource_id))
            .map(|entry| entry.in_flight);
        match pending {
            None => break,
            Some(Some(fence)) => wait_scanout(crate::drm::DrmWait {
                device: file.device.clone(),
                fence,
            })?,
            Some(None) => super::wait_retry(crate::drm::DrmRetry {
                device: file.device.clone(),
                generation,
            })?,
        }
    }
    let reservation = file
        .state
        .lock()
        .handle_ids
        .reserve()
        .map_err(|_| errno::ENOMEM)?;
    let handle = reservation.id();
    let result = (|| {
        let attachment =
            VirglAttachment::prepare(&file.device, context_id, buffer.buffer().clone())
                .map_err(drm_errno)?;
        let entry =
            FallibleMap::try_prepare(handle, attachment.clone()).map_err(|_| errno::ENOMEM)?;
        // 可中断等待前 arm，EINTR/copyout fault 通过 attachment 的预分配节点安全 detach。
        loop {
            match file
                .submit_virgl(crate::drivers::VirglCommand::ContextAttachResource {
                    context_id,
                    resource_id: buffer.buffer().resource_id,
                })
                .map_err(drm_errno)?
            {
                crate::drm::DrmSubmission::Retry(retry) => super::wait_retry(retry)?,
                crate::drm::DrmSubmission::Wait(wait) => {
                    attachment.arm(wait.fence);
                    wait_scanout(wait)?;
                    break;
                }
            }
        }
        write_u32(&mut bytes, 0, handle)?;
        copy_out(user, argument, &bytes)?;
        file.state.lock().graphics_buffers.commit_vacant(entry);
        Ok::<_, isize>(())
    })();
    match result {
        Ok(()) => {
            file.state.lock().handle_ids.publish(reservation);
            Ok(())
        }
        Err(error) => {
            file.state.lock().handle_ids.rollback(reservation);
            Err(error)
        }
    }
}
