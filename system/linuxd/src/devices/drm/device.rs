use crate::sync::Once;

use super::*;
use device_model::{DisplayError, DisplayUpdate, VirglCommand};

enum ScanoutBacking {
    Dumb(Arc<DeviceBacking>),
    Virgl(u32),
}

pub(super) fn display_error(error: DisplayError) -> DrmError {
    match error {
        DisplayError::WouldBlock => DrmError::Busy,
        DisplayError::InvalidRectangle => DrmError::Invalid,
        DisplayError::Device => DrmError::Device,
    }
}

impl DrmFile {
    /// 删除本 OFD 创建的 framebuffer，并显式释放 adapter residency。
    ///
    /// # Parameters
    ///
    /// - `id`: device-wide framebuffer ID。
    ///
    /// # Returns
    ///
    /// object 已删除、disable/RESOURCE_UNREF 的 exact-fence wait token，或 adapter
    /// readiness retry token。
    ///
    /// # Errors
    ///
    /// object 不存在返回 NotFound；adapter failure 返回对应错误。
    pub(crate) fn remove_framebuffer(&self, id: u32) -> Result<FramebufferRemoval, DrmError> {
        let mut completion = self.device.completion.lock();
        {
            let state = self.device.state.lock();
            if state
                .framebuffers
                .get(&id)
                .is_none_or(|framebuffer| framebuffer.owner != self.file_identity)
            {
                return Err(DrmError::NotFound);
            }
        }
        if completion.pending.is_some() || completion.reset_after_owner.is_some() {
            return Ok(FramebufferRemoval::Retry(DrmRetry {
                device: self.device.clone(),
                generation: completion.adapter_generation,
            }));
        }
        if completion.active.as_ref().is_some_and(|active| {
            active
                .framebuffer()
                .is_some_and(|framebuffer| framebuffer.id == id)
        }) {
            return match self.submit_disable(&mut completion) {
                Ok(wait) => Ok(FramebufferRemoval::Wait(wait)),
                Err(DrmError::Busy) => Ok(FramebufferRemoval::Retry(DrmRetry {
                    device: self.device.clone(),
                    generation: completion.adapter_generation,
                })),
                Err(error) => Err(error),
            };
        }
        let is_virgl = {
            let state = self.device.state.lock();
            matches!(
                &state
                    .framebuffers
                    .get(&id)
                    .expect("validated framebuffer disappeared under owner lock")
                    .backing,
                FramebufferBacking::Virgl(_)
            )
        };
        let fence = if is_virgl {
            None
        } else {
            match self.device.display.release_buffer(u64::from(id)) {
                Ok(fence) => fence,
                Err(DisplayError::WouldBlock) => {
                    return Ok(FramebufferRemoval::Retry(DrmRetry {
                        device: self.device.clone(),
                        generation: completion.adapter_generation,
                    }));
                }
                Err(error) => return Err(display_error(error)),
            }
        };
        if let Some(fence) = fence {
            completion
                .timeline
                .submit(fence)
                .expect("published DRM release fence exceeded completion timeline");
            completion.pending = Some(PendingDisplay {
                fence,
                operation: PendingOperation::Release,
            });
            return Ok(FramebufferRemoval::Wait(DrmWait {
                device: self.device.clone(),
                fence,
            }));
        }
        let removed = self
            .device
            .state
            .lock()
            .framebuffers
            .remove(&id)
            .expect("validated framebuffer disappeared under owner lock");
        drop(completion);
        // framebuffer 可能持有 GEM backing 的最后一个 Arc；页回收不得发生在 device
        // object namespace lock 内，否则 close/RMFB 会放大所有 KMS query 的尾延迟。
        drop(removed);
        Ok(FramebufferRemoval::Removed)
    }

    pub(super) fn submit_configuration_locked(
        &self,
        completion: &mut CompletionState,
        configuration: &super::kms::Configuration,
        event: Option<PendingEvent>,
    ) -> Result<DrmWait, DrmError> {
        if !self.is_master() {
            return Err(DrmError::Permission);
        }
        if completion.pending.is_some() || completion.reset_after_owner.is_some() {
            return Err(DrmError::Busy);
        }
        if event.is_some() && self.events.lock().len() == EVENT_QUEUE_CAPACITY {
            return Err(DrmError::OutOfMemory);
        }
        configuration.validate()?;
        if let Some(framebuffer) = configuration.framebuffer() {
            let state = self.device.state.lock();
            if state
                .framebuffers
                .get(&framebuffer.id)
                .is_none_or(|live| !Arc::ptr_eq(live, framebuffer))
            {
                return Err(DrmError::NotFound);
            }
        }

        let fence = if configuration.is_scanning() {
            let mode = configuration.native_mode().ok_or(DrmError::Invalid)?;
            let framebuffer = configuration.framebuffer().ok_or(DrmError::Invalid)?;
            match &framebuffer.backing {
                FramebufferBacking::Dumb(buffer) => self.device.display.submit_scanout(
                    u64::from(framebuffer.id),
                    mode,
                    buffer.backing.clone(),
                ),
                FramebufferBacking::Virgl(buffer) => self.device.display.submit_virgl_scanout(
                    mode,
                    buffer.resource_id,
                    configuration.flush_damage(completion.active.as_ref()),
                ),
            }
        } else {
            self.device.display.disable_scanout()
        }
        .map_err(display_error)?;
        completion
            .timeline
            .submit(fence)
            .expect("published DRM commit fence exceeded completion timeline");
        completion.pending = Some(PendingDisplay {
            fence,
            operation: PendingOperation::Commit {
                next: Some(configuration.clone()),
                event,
                retiring: None,
            },
        });
        Ok(DrmWait {
            device: self.device.clone(),
            fence,
        })
    }

    pub(super) fn submit_damage(
        &self,
        completion: &mut CompletionState,
        framebuffer_id: u32,
        rectangles: &[DisplayRect],
    ) -> Result<DrmWait, DrmError> {
        if completion.pending.is_some() || completion.reset_after_owner.is_some() {
            return Err(DrmError::Busy);
        }
        let (mode, backing, framebuffer) = {
            let state = self.device.state.lock();
            let framebuffer = state
                .framebuffers
                .get(&framebuffer_id)
                .ok_or(DrmError::NotFound)?;
            (
                DisplayMode {
                    width: framebuffer.width,
                    height: framebuffer.height,
                    pitch: framebuffer.pitch,
                },
                match &framebuffer.backing {
                    FramebufferBacking::Dumb(buffer) => {
                        ScanoutBacking::Dumb(buffer.backing.clone())
                    }
                    FramebufferBacking::Virgl(buffer) => ScanoutBacking::Virgl(buffer.resource_id),
                },
                framebuffer.clone(),
            )
        };
        let fence = match backing {
            ScanoutBacking::Dumb(backing) => self.device.display.submit_damage(
                u64::from(framebuffer_id),
                mode,
                backing,
                rectangles,
            ),
            ScanoutBacking::Virgl(resource_id) => {
                if rectangles.len() != 1 {
                    return Err(DrmError::Invalid);
                }
                self.device.display.submit_virgl(VirglCommand::Flush {
                    resource_id,
                    rectangle: rectangles[0],
                })
            }
        }
        .map_err(display_error)?;
        completion
            .timeline
            .submit(fence)
            .expect("published DRM damage fence exceeded completion timeline");
        completion.pending = Some(PendingDisplay {
            fence,
            operation: PendingOperation::Damage { framebuffer },
        });
        Ok(DrmWait {
            device: self.device.clone(),
            fence,
        })
    }

    pub(super) fn submit_disable(
        &self,
        completion: &mut CompletionState,
    ) -> Result<DrmWait, DrmError> {
        if completion.pending.is_some() || completion.reset_after_owner.is_some() {
            return Err(DrmError::Busy);
        }
        let fence = self
            .device
            .display
            .disable_scanout()
            .map_err(display_error)?;
        completion
            .timeline
            .submit(fence)
            .expect("published DRM disable fence exceeded completion timeline");
        completion.pending = Some(PendingDisplay {
            fence,
            operation: PendingOperation::Commit {
                next: None,
                event: None,
                retiring: None,
            },
        });
        Ok(DrmWait {
            device: self.device.clone(),
            fence,
        })
    }
}

impl Drop for DrmFile {
    fn drop(&mut self) {
        // blob 最后引用的析构会摘除 device 弱索引；须先移出 OFD 锁再释放。
        let blobs = core::mem::take(&mut self.state.lock().blobs);
        drop(blobs);
        let identity = self.file_identity;
        // 非活动配置只持 mode/blob，不需要硬件 reset；所有最终载荷析构仍移到锁外。
        let mut retired = (None, None);
        {
            let mut completion = self.device.completion.lock();
            let owned_active = completion
                .active
                .as_ref()
                .is_some_and(|active| active.owner() == identity);
            let incoming =
                completion
                    .pending
                    .as_ref()
                    .and_then(|pending| match &pending.operation {
                        PendingOperation::Commit {
                            next: Some(next), ..
                        } if next.owner() == identity => Some(next.is_scanning()),
                        _ => None,
                    });
            if incoming == Some(true) {
                // 已发表的扫描目标不能撤销；先保活旧目标，完成后 reset 新目标。
                assert!(
                    completion.reset_after_owner.is_none(),
                    "closed scanout reset owner replaced"
                );
                let retiring = completion.active.take();
                completion.reset_after_owner = Some(ClosedScanout {
                    owner: identity,
                    retiring,
                });
            } else if incoming == Some(false) {
                let active = completion.active.take();
                let Some(PendingDisplay {
                    operation: PendingOperation::Commit { next, retiring, .. },
                    ..
                }) = &mut completion.pending
                else {
                    unreachable!()
                };
                assert!(retiring.is_none(), "commit already owns the closed scanout");
                *retiring = active;
                retired.0 = next.take(); // disable 已在途，不能再安排一次 reset。
            } else if owned_active {
                let active = completion.active.take();
                if let Some(PendingDisplay {
                    operation: PendingOperation::Commit { retiring, .. },
                    ..
                }) = &mut completion.pending
                {
                    // 后继所有者的替换会自然退休旧画面，关闭旧 fd 不得清除它的新输出。
                    assert!(retiring.is_none(), "commit already owns the closed scanout");
                    *retiring = active;
                } else if active
                    .as_ref()
                    .is_some_and(super::kms::Configuration::is_scanning)
                {
                    assert!(
                        completion.reset_after_owner.is_none(),
                        "closed scanout reset owner replaced"
                    );
                    completion.reset_after_owner = Some(ClosedScanout {
                        owner: identity,
                        retiring: active,
                    });
                } else {
                    retired.1 = active;
                }
            }
            advance_scanout_reset(&self.device, &mut completion);
        }
        drop(retired);
        {
            let mut state = self.device.state.lock();
            if state.master == Some(identity) {
                state.master = None;
            }
        }
        loop {
            let removed = {
                // 与提交的 namespace 复核串行；引用摘除后仍在 completion 锁外析构。
                let _completion = self.device.completion.lock();
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
        let cleanup = {
            let mut state = self.state.lock();
            let Some(context) = state.context.take() else {
                assert!(state.graphics_buffers.is_empty());
                return;
            };
            let buffers = core::mem::take(&mut state.graphics_buffers);
            context.into_cleanup(buffers)
        };
        self.device
            .state
            .lock()
            .graphics_cleanups
            .commit_vacant(cleanup);
        let mut completion = self.device.completion.lock();
        advance_graphics_cleanup(&self.device, &mut completion);
    }
}

fn advance_scanout_reset(device: &DrmDevice, completion: &mut CompletionState) {
    if completion.reset_after_owner.is_none() || completion.pending.is_some() {
        return;
    }
    let fence = match device.display.disable_scanout() {
        Ok(fence) => fence,
        Err(DisplayError::WouldBlock) => return,
        Err(error) => panic!("closed DRM OFD scanout reset failed: {error:?}"),
    };
    completion
        .timeline
        .submit(fence)
        .expect("close reset exceeded completion timeline");
    let closed = completion
        .reset_after_owner
        .take()
        .expect("accepted reset lost owner");
    completion.pending = Some(PendingDisplay {
        fence,
        operation: PendingOperation::Commit {
            next: None,
            event: None,
            retiring: closed.retiring,
        },
    });
}

pub(super) fn advance_graphics_cleanup(device: &DrmDevice, completion: &mut CompletionState) {
    advance_scanout_reset(device, completion);
    // reset 排在资源 detach/UNREF 前；否则关闭 primary 后 host 仍可能扫描已释放资源。
    if completion.reset_after_owner.is_some() {
        return;
    }
    let detachment = {
        let state = device.state.lock();
        state.graphics_detachments.iter().find_map(|(&key, entry)| {
            (entry.in_flight.is_none() && entry.wait_fence <= completion.timeline.completed())
                .then_some((key, entry.context_id, entry.resource_id))
        })
    };
    if let Some((key, context_id, resource_id)) = detachment {
        let fence =
            match device
                .display
                .submit_virgl(device_model::VirglCommand::ContextDetachResource {
                    context_id,
                    resource_id,
                }) {
                Ok(fence) => fence,
                Err(DisplayError::WouldBlock) => return,
                Err(error) => panic!("VirGL detach failed: {error:?}"),
            };
        completion
            .timeline
            .submit(fence)
            .expect("detach exceeded completion timeline");
        device
            .state
            .lock()
            .graphics_detachments
            .get_mut(&key)
            .expect("selected detach disappeared")
            .in_flight = Some(fence);
        return;
    }
    let retirement = {
        let state = device.state.lock();
        state
            .graphics_retirements
            .iter()
            .find_map(|(&id, retirement)| {
                let buffer = retirement
                    .buffer
                    .as_ref()
                    .expect("queued retirement lost buffer");
                (retirement.in_flight.is_none()
                    && Arc::strong_count(buffer) == 1
                    && buffer.lifecycle.lock().fence <= completion.timeline.completed())
                .then_some(id)
            })
    };
    if let Some(resource_id) = retirement {
        let fence = match device
            .display
            .submit_virgl(device_model::VirglCommand::ResourceUnref { resource_id })
        {
            Ok(fence) => fence,
            Err(DisplayError::WouldBlock) => return,
            Err(error) => panic!("VirGL resource retirement failed: {error:?}"),
        };
        completion
            .timeline
            .submit(fence)
            .expect("retirement cleanup exceeded completion timeline");
        device
            .state
            .lock()
            .graphics_retirements
            .get_mut(&resource_id)
            .expect("selected retirement disappeared")
            .in_flight = Some(fence);
        return;
    }
    let selected = {
        let state = device.state.lock();
        state
            .graphics_cleanups
            .iter()
            .find_map(|(&context, cleanup)| {
                let action = cleanup.next_action(completion.timeline.completed())?;
                if matches!(action, super::graphics::VirglCleanupAction::Context { .. })
                    && state
                        .graphics_detachments
                        .iter()
                        .any(|(_, retirement)| retirement.context_id == context)
                {
                    return None;
                }
                Some((context, action))
            })
    };
    let Some((context, action)) = selected else {
        return;
    };
    let fence = match device.display.submit_virgl(action.command()) {
        Ok(fence) => fence,
        Err(DisplayError::WouldBlock) => return,
        Err(error) => panic!("VirGL OFD cleanup submission failed: {error:?}"),
    };
    completion
        .timeline
        .submit(fence)
        .expect("published VirGL cleanup fence exceeded completion timeline");
    device
        .state
        .lock()
        .graphics_cleanups
        .get_mut(&context)
        .expect("selected VirGL cleanup disappeared")
        .record_submission(action, fence);
}

fn complete_graphics_cleanup(device: &DrmDevice, fence: u64) {
    let detached = {
        let mut state = device.state.lock();
        let key = state
            .graphics_detachments
            .iter()
            .find_map(|(&key, entry)| (entry.in_flight == Some(fence)).then_some(key));
        key.and_then(|key| state.graphics_detachments.take_entry(&key))
    };
    if let Some(detached) = detached {
        drop(detached);
        return;
    }
    let retirement = {
        let mut state = device.state.lock();
        let id = state
            .graphics_retirements
            .iter()
            .find_map(|(&id, retirement)| (retirement.in_flight == Some(fence)).then_some(id));
        id.and_then(|id| state.graphics_retirements.take_entry(&id))
    };
    if let Some(mut retirement) = retirement {
        if let Some(reservation) = retirement.value_mut().reservation.take() {
            device
                .state
                .lock()
                .graphics_resource_ids
                .rollback(reservation);
        }
        // 释放 backing 在 device state 锁外；host exact UNREF fence 已完成。
        drop(retirement);
        return;
    }
    let context = {
        let mut state = device.state.lock();
        let context = state
            .graphics_cleanups
            .iter()
            .find_map(|(&context, cleanup)| cleanup.owns_fence(fence).then_some(context));
        let Some(context) = context else {
            return;
        };
        match state
            .graphics_cleanups
            .get_mut(&context)
            .expect("matched VirGL cleanup disappeared")
            .complete(fence)
        {
            Some(super::graphics::VirglCleanupCompletion::Context) => {
                state.graphics_cleanups.take_entry(&context)
            }
            None => None,
        }
    };
    // AVL node、Arc 和 backing 均在 device state guard 外释放，避免持锁进入物理页回收。
    drop(context);
}

// OWNER: DRM module 唯一拥有 primary KMS device；devfs/OFD 后续只持该 owner 的 Arc 投影。
// 缺失单例会让多个 card0 实例竞争同一 hardware scanout 与 completion queue。
static PRIMARY_DRM: Once<Arc<DrmDevice>> = Once::new();

impl crate::fs::GraphicsSysfsSource for DrmDevice {
    fn identity(&self) -> Option<(u64, u32)> {
        self.display.platform_identity()
    }
}

/// 从通用 display seam 与统一 wait notification Pipe 初始化 primary DRM owner。
///
/// # Parameters
///
/// - `display`: DTB 选中的唯一 single-scanout adapter。
///
/// # Returns
///
/// owner 成功发布时返回 unit。
///
/// # Errors
///
/// 重复初始化或内存不足返回 unit error。
pub(crate) fn init(display: Arc<dyn GraphicsDevice>) -> Result<(), ()> {
    if PRIMARY_DRM.get().is_some() {
        return Err(());
    }
    // completion_read 只由 DRM waiter 排空；completion_write 由 deferred completion 发布。
    let (completion_read, completion_write) = crate::ipc::Pipe::notification_pair()?;
    let mode = display.mode();
    let resource_start = display.first_dynamic_resource_id();
    let owner = Arc::try_new(DrmDevice {
        display,
        completion_read,
        completion_write,
        completion: Mutex::new(CompletionState {
            pending: None,
            active: None,
            timeline: FenceTimeline::new(),
            cursor_completed: 0,
            adapter_generation: 0,
            sequence: 0,
            reset_after_owner: None,
        }),
        state: Mutex::new(DrmDeviceState {
            buffer_identities: PublicationIdAllocator::new(1),
            next_file_identity: 1,
            object_ids: PublicationIdAllocator::new(super::kms::FIRST_DYNAMIC_ID),
            context_ids: PublicationIdAllocator::new(1),
            // adapter 唯一声明其保留前缀，包括仍存活的 crash framebuffer。
            graphics_resource_ids: PublicationIdAllocator::new(resource_start),
            graphics_cleanups: FallibleMap::new(),
            graphics_retirements: FallibleMap::new(),
            graphics_detachments: FallibleMap::new(),
            master: None,
            mode,
            framebuffers: FallibleMap::new(),
            blobs: FallibleMap::new(),
        }),
    })
    .map_err(|_| ())?;
    let display = owner.display.clone();
    crate::fs::register_graphics_sysfs(owner.clone()).map_err(|_| ())?;
    PRIMARY_DRM.call_once(|| owner);
    display.bind_activity(crate::deferred::activity(crate::deferred::register(
        display_work,
    )?)?);
    let driver = Arc::try_new(super::card_file::CardDriver { render: false }).map_err(|_| ())?;
    crate::fs::chardev::register_driver(super::card_file::CARD_NUMBER, 1, driver)
        .map_err(|_| ())?;
    crate::fs::devnode::register_node(
        super::card_file::CARD_PATH,
        super::card_file::CARD_NUMBER,
        0o666,
    )
    .map_err(|_| ())?;
    let render = crate::fs::DeviceNumber::new(226, 128);
    crate::fs::chardev::register_driver(
        render,
        1,
        Arc::try_new(super::card_file::CardDriver { render: true }).map_err(|_| ())?,
    )
    .map_err(|_| ())?;
    crate::fs::devnode::register_node(b"dri/renderD128", render, 0o666).map_err(|_| ())
}

/// 打开 primary DRM card 的新 OFD backend。
///
/// # Returns
///
/// 共享 hardware owner、独立 file identity 的 backend。
///
/// # Errors
///
/// primary DRM 未初始化或 control block OOM 返回 unit error。
pub(super) fn open(render_node: bool) -> Result<Arc<DrmFile>, ()> {
    let device = PRIMARY_DRM.get().cloned().ok_or(())?;
    let file_identity = {
        let mut state = device.state.lock();
        let identity = state.next_file_identity;
        state.next_file_identity = identity.checked_add(1).ok_or(())?;
        identity
    };
    let file = Arc::try_new(DrmFile {
        device,
        file_identity,
        render_node,
        context_creation: crate::sync::TaskMutex::new(()),
        state: Mutex::new(DrmFileState {
            handle_ids: PublicationIdAllocator::new(1),
            buffers: FallibleMap::new(),
            blobs: FallibleMap::new(),
            context: None,
            graphics_buffers: FallibleMap::new(),
            was_master: false,
            universal_planes: false,
            atomic: false,
        }),
        events: Mutex::new(EventQueue::new()),
    })
    .map_err(|_| ())?;
    let mut state = file.device.state.lock();
    if !render_node && state.master.is_none() {
        state.master = Some(file_identity);
        file.state.lock().was_master = true;
    }
    drop(state);
    Ok(file)
}

/// 在 deferred context 有界推进一次 GPU controlq completion。
///
/// # Parameters
///
/// - `timestamp_ns`: task deferred owner 在本批次取得的 monotonic completion 时刻。
///
/// # Returns
///
/// 无返回值；每个 IRQ 只推进一个 resource transaction stage。
///
/// # Errors
///
/// 未初始化、descriptor/fence 损坏或 device failure 直接 fail-stop。
fn display_work(timestamp_ns: u64) -> bool {
    dispatch_display_work(timestamp_ns);
    false
}

fn dispatch_display_work(timestamp_ns: u64) {
    let drm = PRIMARY_DRM
        .get()
        .expect("display softirq arrived before DRM initialization");
    // completion lock 必须先于 adapter controlq lock；submit path 使用同一顺序，保证
    // notify 后立即到达的 IRQ 不会在 pending fence publication 前完成归属。
    let mut state = drm.completion.lock();
    let update = drm
        .display
        .poll_update()
        .unwrap_or_else(|error| match error {
            DisplayError::WouldBlock | DisplayError::InvalidRectangle | DisplayError::Device => {
                panic!("display completion failed: {:?}", error)
            }
        });
    let Some(update) = update else {
        return;
    };
    state.adapter_generation = state.adapter_generation.wrapping_add(1);
    let fence = match update {
        DisplayUpdate::CursorCompleted(sequence) => {
            assert!(
                sequence > state.cursor_completed,
                "duplicate or non-monotonic cursor completion"
            );
            state.cursor_completed = sequence;
            drop(state);
            drm.completion_write.signal_readiness();
            return;
        }
        DisplayUpdate::OperationCompleted(fence) => fence,
        DisplayUpdate::RenderCompleted(fence)
            if state
                .pending
                .as_ref()
                .is_some_and(|pending| pending.fence == fence) =>
        {
            fence
        }
        DisplayUpdate::RenderCompleted(fence) => {
            state
                .timeline
                .complete(fence)
                .expect("unknown or duplicate VirGL completion fence");
            complete_graphics_cleanup(drm, fence);
            advance_graphics_cleanup(drm, &mut state);
            drop(state);
            drm.completion_write.signal_readiness();
            return;
        }
        DisplayUpdate::ModeChanged(mode) => {
            drop(state);
            publish_mode_change(drm, mode);
            return;
        }
        DisplayUpdate::AdapterReady => {
            advance_graphics_cleanup(drm, &mut state);
            drop(state);
            drm.completion_write.signal_readiness();
            return;
        }
    };
    let pending = state
        .pending
        .take()
        .expect("display completion without pending DRM transaction");
    assert_eq!(pending.fence, fence);
    state
        .timeline
        .complete(fence)
        .expect("unknown or duplicate DRM completion fence");
    // OWNER: completion 中升级的 OFD 引用必须活到锁外；最后一个引用的 Drop 会重新取得 completion 锁。
    let mut event_file = None;
    let mut retired_framebuffer = None;
    let retired = match pending.operation {
        PendingOperation::Commit {
            next,
            event,
            retiring,
        } => {
            if next.is_some() || event.is_some() {
                state.sequence = state.sequence.wrapping_add(1);
            }
            if let Some(event) = event {
                event_file = event.file.upgrade();
                if let Some(file) = &event_file {
                    file.events.lock().push(DrmEvent {
                        user_data: event.user_data,
                        seconds: (timestamp_ns / 1_000_000_000) as u32,
                        microseconds: (timestamp_ns % 1_000_000_000 / 1_000) as u32,
                        sequence: state.sequence,
                    });
                }
            }
            match next {
                Some(next)
                    if state
                        .reset_after_owner
                        .as_ref()
                        .is_some_and(|closed| closed.owner == next.owner()) =>
                {
                    // 候选目标已经实际选中，接替 reset intent 中原来的硬件目标。
                    let previous = state
                        .reset_after_owner
                        .as_mut()
                        .expect("closed scanout lost reset intent")
                        .retiring
                        .replace(next);
                    (state.active.take(), previous, retiring)
                }
                Some(next) => (state.active.replace(next), None, retiring),
                None => (state.active.take(), None, retiring),
            }
        }
        PendingOperation::Damage { framebuffer, .. } => {
            retired_framebuffer = Some(framebuffer);
            (None, None, None)
        }
        PendingOperation::Release => (None, None, None),
    };
    drop(state);
    drop(retired);
    drop(retired_framebuffer);
    drop(event_file);
    // 退休后才推进 UNREF：否则新释放的最后一个 framebuffer pin 要等无关 IRQ 才能被回收。
    let mut state = drm.completion.lock();
    advance_graphics_cleanup(drm, &mut state);
    drop(state);
    drm.completion_write.signal_readiness();
}

fn publish_mode_change(drm: &DrmDevice, mode: DisplayMode) {
    let mut state = drm.state.lock();
    if state.mode == mode {
        drop(state);
        drm.completion_write.signal_readiness();
        return;
    }
    state.mode = mode;
    drop(state);
    crate::net::publish_drm_hotplug();
    drm.completion_write.signal_readiness();
    info!("display mode changed to {}x{}", mode.width, mode.height);
}
