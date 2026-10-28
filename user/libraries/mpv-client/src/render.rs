//! 渲染线程独占 mpv_render_*；回调只投递 Update，不能等待执行普通 client API 的 UI 线程。
use crate::{
    check,
    ffi::{self, Api, Handle, Param, RenderContext},
};
use std::{
    ffi::c_void,
    io::{self, Write},
    os::unix::net::UnixStream,
    ptr,
    sync::mpsc::{self, Sender},
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

enum Request {
    Update,
    Ready(Sender<bool>),
    Draw {
        width: u32,
        height: u32,
        pixels: Vec<u32>,
        reply: Sender<(Vec<u32>, io::Result<()>)>,
    },
    Stop,
}
struct Wake {
    requests: Sender<Request>,
    writer: UnixStream,
}

extern "C" fn wake(pointer: *mut c_void) {
    // SAFETY: State 持有地址稳定的 Box，撤销 callback 并等待回调退出后才析构它。
    let wake = unsafe { &*pointer.cast::<Wake>() };
    // Sender::send 不等待 receiver；不能在 libmpv callback 中调用 render API 或等待 UI。
    let _ = wake.requests.send(Request::Update);
}

struct State {
    api: Api,
    context: *mut RenderContext,
    wake: Box<Wake>,
    ready: bool,
}
impl State {
    fn create(
        api: Api,
        core: CoreHandle,
        requests: Sender<Request>,
        writer: UnixStream,
    ) -> io::Result<Self> {
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
            wake: Box::new(Wake { requests, writer }),
            ready: false,
        };
        let pointer = ptr::addr_of_mut!(*state.wake).cast();
        // SAFETY: callback 数据的 Box 不再移动；Drop 先撤销 callback 再释放 context 和数据。
        unsafe { (api.render_set_update_callback)(context, Some(wake), pointer) };
        Ok(state)
    }

    fn update(&mut self) {
        // SAFETY: 全部 render API 调用由本线程串行执行，永不在 callback 中调用。
        self.ready |= unsafe { (self.api.render_update)(self.context) & ffi::UPDATE_FRAME != 0 };
        if self.ready {
            let _ = (&self.wake.writer).write(&[1]);
        }
    }

    fn draw(&self, width: u32, height: u32, pixels: &mut Vec<u32>) -> io::Result<()> {
        let count = (width as usize)
            .checked_mul(height as usize)
            .filter(|_| width != 0 && height != 0)
            .ok_or(io::ErrorKind::InvalidInput)?;
        let mut size = [
            i32::try_from(width).map_err(io::Error::other)?,
            i32::try_from(height).map_err(io::Error::other)?,
        ];
        pixels.resize(count, 0);
        let mut stride = width as usize * 4;
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
                data: pixels.as_mut_ptr().cast(),
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
    requests: Sender<Request>,
    thread: Option<JoinHandle<()>>,
    pixels: Vec<u32>,
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
        let (requests, receiver) = mpsc::channel();
        let callback_requests = requests.clone();
        let (started, startup) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("mpv-render".into())
            .spawn(move || {
                let mut state = match State::create(api, core, callback_requests, writer) {
                    Ok(state) => state,
                    Err(error) => {
                        let _ = started.send(Err(error));
                        return;
                    }
                };
                if started.send(Ok(())).is_err() {
                    return;
                }
                while let Ok(request) = receiver.recv() {
                    match request {
                        Request::Update => state.update(),
                        Request::Ready(reply) => {
                            let _ = reply.send(std::mem::take(&mut state.ready));
                        }
                        Request::Draw {
                            width,
                            height,
                            mut pixels,
                            reply,
                        } => {
                            let result = state.draw(width, height, &mut pixels);
                            let _ = reply.send((pixels, result));
                        }
                        Request::Stop => break,
                    }
                }
            })?;
        match startup
            .recv()
            .unwrap_or_else(|_| Err(io::Error::other("mpv render thread stopped during startup")))
        {
            Ok(()) => Ok(Self {
                requests,
                thread: Some(thread),
                pixels: Vec::new(),
            }),
            Err(error) => {
                let _ = thread.join();
                Err(error)
            }
        }
    }

    pub(super) fn ready(&self) -> bool {
        let (reply, receiver) = mpsc::channel();
        self.requests.send(Request::Ready(reply)).is_ok() && receiver.recv().unwrap_or(false)
    }

    pub(super) fn render(&mut self, width: u32, height: u32) -> io::Result<&[u32]> {
        let (reply, receiver) = mpsc::channel();
        self.requests
            .send(Request::Draw {
                width,
                height,
                pixels: std::mem::take(&mut self.pixels),
                reply,
            })
            .map_err(|_| io::Error::other("mpv render thread stopped"))?;
        let (pixels, result) = receiver
            .recv()
            .map_err(|_| io::Error::other("mpv render thread stopped"))?;
        self.pixels = pixels;
        result?;
        Ok(&self.pixels)
    }
}
impl Drop for Renderer {
    fn drop(&mut self) {
        // callback 自持 sender，所以不能依赖断开 channel 退出；Stop + join 是销毁 core 前的明确屏障。
        let _ = self.requests.send(Request::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
