use spin::Once;

use super::*;

impl Drop for DrmFile {
    fn drop(&mut self) {
        let identity = self.file_identity;
        {
            let mut completion = self.device.completion.lock();
            let owned_active = completion
                .active
                .is_some_and(|active| active.owner == identity);
            if completion
                .pending
                .as_ref()
                .is_some_and(|pending| pending.owner == Some(identity))
            {
                completion.reset_after_owner = Some(identity);
            } else if completion.pending.is_none() && owned_active {
                self.submit_scanout(&mut completion, None)
                    .expect("closing DRM OFD failed to restore fallback scanout");
            }
            if owned_active {
                // close 后 object ID 立即离开可查询 namespace；hardware 可能仍显示旧
                // backing 到已排队 transaction 完成，但不得发布指向已删除 object 的 ID。
                completion.active = None;
            }
        }
        loop {
            let removed = {
                let mut state = self.device.state.lock();
                let id = state
                    .framebuffers
                    .iter()
                    .find_map(|(&id, framebuffer)| (framebuffer.owner == identity).then_some(id));
                id.and_then(|id| state.framebuffers.remove(&id))
            };
            let Some(framebuffer) = removed else {
                break;
            };
            // 每轮先释放 namespace lock 再析构 backing；使用迭代摘除而非临时 Vec，
            // 保证 close 在 OOM 路径仍不分配，也不把 allocator lock 嵌套进 DRM lock。
            drop(framebuffer);
        }
    }
}

// OWNER: DRM module 唯一拥有 primary KMS device；devfs/OFD 后续只持该 owner 的 Arc 投影。
// 缺失单例会让多个 card0 实例竞争同一 hardware scanout 与 completion queue。
static PRIMARY_DRM: Once<Arc<DrmDevice>> = Once::new();

/// @description 从通用 display seam 与统一 wait notification Pipe 初始化 primary DRM owner。
///
/// @param display DTB 选中的唯一 single-scanout adapter。
/// @param completion_read 只由 DRM waiter 排空的 notification endpoint。
/// @param completion_write deferred completion 发布 endpoint。
/// @return owner 成功发布时返回 unit。
/// @errors 重复初始化或内存不足返回 unit error。
pub(crate) fn init(
    display: Arc<dyn DisplayDevice>,
    completion_read: Arc<PipeEnd>,
    completion_write: Arc<PipeEnd>,
) -> Result<(), ()> {
    if PRIMARY_DRM.get().is_some() {
        return Err(());
    }
    let mode = display.mode();
    let fallback_backing = display.initial_backing();
    let owner = Arc::try_new(DrmDevice {
        display,
        _mode: mode,
        fallback_backing,
        completion_read,
        completion_write,
        completion: Mutex::new(CompletionState {
            pending: None,
            active: None,
            completed: 0,
            reset_after_owner: None,
        }),
        state: Mutex::new(DrmDeviceState {
            next_buffer_identity: 1,
            next_file_identity: 1,
            next_framebuffer_id: 4,
            framebuffers: FallibleMap::new(),
        }),
    })
    .map_err(|_| ())?;
    PRIMARY_DRM.call_once(|| owner);
    Ok(())
}

/// @description 打开 primary DRM card 的新 OFD backend。
/// @return 共享 hardware owner、独立 file identity 的 backend。
/// @errors primary DRM 未初始化或 control block OOM 返回 unit error。
pub(crate) fn open() -> Result<Arc<DrmFile>, ()> {
    let device = PRIMARY_DRM.get().cloned().ok_or(())?;
    let file_identity = {
        let mut state = device.state.lock();
        let identity = state.next_file_identity;
        state.next_file_identity = identity.checked_add(1).ok_or(())?;
        identity
    };
    Arc::try_new(DrmFile {
        device,
        file_identity,
        state: Mutex::new(DrmFileState {
            next_handle: 1,
            buffers: FallibleMap::new(),
        }),
    })
    .map_err(|_| ())
}

/// @description 在 deferred context 有界推进一次 GPU controlq completion。
///
/// @return 无返回值；每个 IRQ 只推进一个 resource transaction stage。
/// @errors 未初始化、descriptor/fence 损坏或 device failure 直接 fail-stop。
pub(crate) fn dispatch_display_work() {
    let drm = PRIMARY_DRM
        .get()
        .expect("display softirq arrived before DRM initialization");
    // completion lock 必须先于 adapter controlq lock；submit path 使用同一顺序，保证
    // notify 后立即到达的 IRQ 不会在 pending fence publication 前完成归属。
    let mut state = drm.completion.lock();
    let completion = drm
        .display
        .poll_completion()
        .unwrap_or_else(|error| match error {
            DisplayError::WouldBlock | DisplayError::InvalidRectangle | DisplayError::Device => {
                panic!("display completion failed: {:?}", error)
            }
        });
    let Some(fence) = completion else {
        return;
    };
    let pending = state
        .pending
        .take()
        .expect("display completion without pending DRM transaction");
    assert_eq!(pending.fence, fence);
    state.completed = state.completed.max(fence);
    let reset_after_close = pending
        .owner
        .is_some_and(|owner| state.reset_after_owner == Some(owner));
    state.active = match (pending.framebuffer, pending.owner) {
        (Some(framebuffer), Some(owner)) if !reset_after_close => {
            Some(ActiveScanout { framebuffer, owner })
        }
        _ => None,
    };
    if reset_after_close {
        state.reset_after_owner = None;
        let reset_fence = drm
            .display
            .submit_scanout(drm.fallback_backing.clone())
            .expect("closed DRM OFD failed to queue fallback scanout");
        state.pending = Some(PendingScanout {
            fence: reset_fence,
            framebuffer: None,
            owner: None,
        });
    }
    drop(state);
    drm.completion_write.signal_readiness();
    debug!(
        "[DRM] asynchronous scanout completed, fence={fence}, framebuffer={:?}",
        pending.framebuffer
    );
}
