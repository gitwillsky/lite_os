//! 渲染线程独占 mpv_render_*；UI 只交换最新尺寸与已完成帧，双方不等待对方执行 libmpv。
use crate::{
    FRAME_SLOTS, Frame, check,
    ffi::{self, Api, Handle, Param, RenderContext},
};
use linux_uapi::shared_memory::{MemFd, SharedMapping};
use std::{
    ffi::c_void,
    io::{self, Write},
    os::{fd::AsFd, unix::net::UnixStream},
    ptr,
    sync::{
        Arc, Mutex,
        mpsc::{self, SyncSender},
    },
    thread::{self, JoinHandle},
};

struct CoreHandle(*mut Handle);
// SAFETY: libmpv 允许跨线程创建 render context；Mpv 在销毁 core/卸载 library 前必须停止并 join 此线程。
// 渲染线程不调用普通 client API，指针只传入 mpv_render_context_create，绝不用于销毁 core。
unsafe impl Send for CoreHandle {}
impl CoreHandle {
    fn as_ptr(&self) -> *mut Handle {
        self.0
    }
}

/// 帧交接只在锁内交换所有权。归还槽来自 UI 丢弃的帧或 Wayland buffer.release。
#[derive(Default)]
struct Mailbox {
    size: Option<(u32, u32)>,
    completed: Option<io::Result<crate::Frame>>,
    spare: [Option<Frame>; FRAME_SLOTS],
    stop: bool,
}
impl Mailbox {
    fn recycle(&mut self, frame: Frame) {
        let slot = frame.slot as usize;
        debug_assert!(self.spare[slot].is_none());
        self.spare[slot] = Some(frame);
    }
}
struct Wake {
    requests: SyncSender<()>,
}

extern "C" fn wake(pointer: *mut c_void) {
    // SAFETY: State 持有地址稳定的 Box，撤销 callback 并等待回调退出后才析构它。
    let wake = unsafe { &*pointer.cast::<Wake>() };
    // 容量为 1 的通知只表示“有工作”；满时已有唤醒，不积压回调，也不等待 UI。
    let _ = wake.requests.try_send(());
}

struct State {
    api: Api,
    context: *mut RenderContext,
    wake: Box<Wake>,
}
impl State {
    fn create(api: Api, core: CoreHandle, requests: SyncSender<()>) -> io::Result<Self> {
        let mut params = [
            Param {
                kind: ffi::PARAM_API_TYPE,
                data: c"sw".as_ptr().cast_mut().cast(),
            },
            Param {
                kind: ffi::PARAM_INVALID,
                data: ptr::null_mut(),
            },
        ];
        let mut context = ptr::null_mut();
        // SAFETY: core 直到本线程 join 后才销毁，参数在调用期间存活，输出只由本线程持有。
        check(&api, unsafe {
            (api.render_create)(&mut context, core.as_ptr(), params.as_mut_ptr())
        })?;
        let mut state = Self {
            api,
            context,
            wake: Box::new(Wake { requests }),
        };
        let pointer = ptr::addr_of_mut!(*state.wake).cast();
        // SAFETY: callback 数据的 Box 不再移动；Drop 先撤销 callback 再释放 context 和数据。
        unsafe { (api.render_set_update_callback)(context, Some(wake), pointer) };
        Ok(state)
    }

    fn update(&mut self) -> bool {
        // SAFETY: 全部 render API 调用由本线程串行执行，永不在 callback 中调用。
        unsafe { (self.api.render_update)(self.context) & ffi::UPDATE_FRAME != 0 }
    }

    fn draw(&self, frame: &mut Frame) -> io::Result<()> {
        let mut size = [frame.width as i32, frame.height as i32];
        let mut stride = frame.stride as usize;
        let mut params = [
            Param {
                kind: ffi::PARAM_SW_SIZE,
                data: size.as_mut_ptr().cast(),
            },
            Param {
                kind: ffi::PARAM_SW_FORMAT,
                data: c"bgr0".as_ptr().cast_mut().cast(),
            },
            Param {
                kind: ffi::PARAM_SW_STRIDE,
                data: ptr::addr_of_mut!(stride).cast(),
            },
            Param {
                kind: ffi::PARAM_SW_POINTER,
                data: frame.mapping.as_non_null().as_ptr().cast(),
            },
            Param {
                kind: ffi::PARAM_INVALID,
                data: ptr::null_mut(),
            },
        ];
        // SAFETY: 缓冲区独占且有 stride*height 字节，所有参数与缓冲区存活至同步调用返回。
        check(&self.api, unsafe {
            (self.api.render_render)(self.context, params.as_mut_ptr())
        })?;
        Ok(())
    }
}
impl Drop for State {
    fn drop(&mut self) {
        // SAFETY: 此线程独占 context；先撤销并同步 callback，再释放 context，最后释放 Box。
        unsafe {
            (self.api.render_set_update_callback)(self.context, None, ptr::null_mut());
            (self.api.render_free)(self.context);
        }
    }
}

pub(super) struct Renderer {
    requests: SyncSender<()>,
    mailbox: Arc<Mutex<Mailbox>>,
    thread: Option<JoinHandle<()>>,
}
impl Renderer {
    /// 创建唯一渲染 owner；失败时 join 已启动线程，调用者才能销毁 core。
    ///
    /// # Safety
    /// core 已初始化，api/library 在 Renderer 的 Drop 完成前必须存活；调用者不得另建 render context。
    pub(super) unsafe fn start(
        api: Api,
        core: *mut Handle,
        writer: UnixStream,
    ) -> io::Result<Self> {
        let core = CoreHandle(core);
        let (requests, receiver) = mpsc::sync_channel(1);
        let callback_requests = requests.clone();
        let mailbox = Arc::new(Mutex::new(Mailbox::default()));
        let rendering = mailbox.clone();
        let (started, startup) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("mpv-render".into())
            .spawn(move || {
                let mut state = match State::create(api, core, callback_requests) {
                    Ok(state) => state,
                    Err(error) => {
                        let _ = started.send(Err(error));
                        return;
                    }
                };
                if started.send(Ok(())).is_err() {
                    return;
                }
                let mut allocated = 0;
                let mut generation = 0;
                let mut rendered_size = None;
                let mut pending = false;
                while receiver.recv().is_ok() {
                    pending |= state.update();
                    let (size, frame, slot) = {
                        let Ok(mut mailbox) = rendering.lock() else {
                            break;
                        };
                        if mailbox.stop {
                            break;
                        }
                        let Some(size) = mailbox.size else {
                            rendered_size = None;
                            continue;
                        };
                        if !pending && rendered_size == Some(size) {
                            continue;
                        }
                        let spare = mailbox.spare.iter_mut().find_map(Option::take);
                        let spare = spare.or_else(|| {
                            if allocated == FRAME_SLOTS && matches!(mailbox.completed, Some(Ok(_)))
                            {
                                mailbox.completed.take().and_then(Result::ok)
                            } else {
                                None
                            }
                        });
                        let slot = match spare.as_ref() {
                            Some(frame) => frame.slot,
                            None if allocated < FRAME_SLOTS => allocated as u32,
                            None => continue,
                        };
                        (size, spare, slot)
                    };
                    let (width, height) = size;
                    let mut frame = if frame
                        .as_ref()
                        .is_some_and(|frame| (frame.width, frame.height) == size)
                    {
                        frame.expect("matching frame")
                    } else {
                        generation += 1;
                        match new_frame(slot, generation, width, height) {
                            Ok(replacement) => replacement,
                            Err(error) => {
                                if let Ok(mut mailbox) = rendering.lock() {
                                    mailbox.spare[slot as usize] = frame;
                                    if let Some(Ok(old)) = mailbox.completed.replace(Err(error)) {
                                        mailbox.recycle(old);
                                    }
                                }
                                let _ = (&writer).write(&[1]);
                                continue;
                            }
                        }
                    };
                    allocated = allocated.max(slot as usize + 1);
                    let rendered = state.draw(&mut frame);
                    let Ok(mut mailbox) = rendering.lock() else {
                        break;
                    };
                    let completed = if let Err(error) = rendered {
                        mailbox.spare[slot as usize] = Some(frame);
                        Err(error)
                    } else {
                        Ok(frame)
                    };
                    if let Some(Ok(old)) = mailbox.completed.replace(completed) {
                        mailbox.recycle(old);
                    }
                    pending = false;
                    rendered_size = Some(size);
                    drop(mailbox);
                    let _ = (&writer).write(&[1]);
                }
            })?;
        match startup
            .recv()
            .unwrap_or_else(|_| Err(io::Error::other("mpv render thread stopped during startup")))
        {
            Ok(()) => Ok(Self {
                requests,
                mailbox,
                thread: Some(thread),
            }),
            Err(error) => {
                let _ = thread.join();
                Err(error)
            }
        }
    }

    pub(super) fn ready(&self) -> bool {
        self.thread.as_ref().is_some_and(JoinHandle::is_finished)
            || self
                .mailbox
                .lock()
                .map_or(true, |mailbox| mailbox.completed.is_some())
    }

    pub(super) fn render(&mut self, width: u32, height: u32) -> io::Result<Option<Frame>> {
        if width == 0 || height == 0 || width > 16384 || height > 16384 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        if self.thread.as_ref().is_none_or(JoinHandle::is_finished) {
            return Err(io::Error::other("mpv render thread stopped"));
        }
        let mut mailbox = self
            .mailbox
            .lock()
            .map_err(|_| io::Error::other("mpv render mailbox poisoned"))?;
        let changed = mailbox.size != Some((width, height));
        mailbox.size = Some((width, height));
        let completed = mailbox.completed.take();
        drop(mailbox);
        if changed {
            // callback 与尺寸变化共用一个有界唤醒，渲染线程总是读取最新尺寸。
            let _ = self.requests.try_send(());
        }
        completed.transpose()
    }

    pub(super) fn recycle(&mut self, frame: Frame) {
        if let Ok(mut mailbox) = self.mailbox.lock() {
            mailbox.recycle(frame);
        }
        let _ = self.requests.try_send(());
    }

    pub(super) fn clear_size(&mut self) {
        if let Ok(mut mailbox) = self.mailbox.lock() {
            mailbox.size = None;
            if let Some(Ok(frame)) = mailbox.completed.take() {
                mailbox.recycle(frame);
            }
        }
        let _ = self.requests.try_send(());
    }
}
fn new_frame(slot: u32, generation: u64, width: u32, height: u32) -> io::Result<Frame> {
    // libmpv recommends 64-byte alignment for both the line pitch and address.
    // mmap supplies a page-aligned base; every line, including the last, owns its padding.
    let stride = (width * 4).next_multiple_of(64);
    let bytes = stride as usize * height as usize;
    let memory = MemFd::create("mpv-video-frame", bytes)?;
    memory.preallocate()?;
    let mapping = SharedMapping::map(memory.as_fd(), bytes)?;
    Ok(Frame {
        slot,
        generation,
        width,
        height,
        stride,
        memory,
        mapping,
    })
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // callback 自持 sender，所以不能依赖断开 channel 退出。stop 与最新尺寸
        // 同属 mailbox；即使通知已满，下一次处理也会看到 stop。join 后才能销毁 core。
        if let Ok(mut mailbox) = self.mailbox.lock() {
            mailbox.stop = true;
        }
        let _ = self.requests.try_send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_target_has_aligned_rows_with_writable_last_row_padding() {
        let frame = new_frame(0, 1, 65, 3).unwrap();
        assert_eq!(frame.stride, 320);
        assert_eq!(frame.mapping.as_non_null().as_ptr() as usize % 64, 0);
        assert_eq!(frame.mapping.len(), 960);
        assert_eq!(frame.memory.len(), 960);
    }
}
