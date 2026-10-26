//! Rust-first 应用 SDK：共享控件、RAII 和 async/await，不依赖系统 Rust dylib 或同一工具链。
mod background;
mod clipboard;
mod dialogs;
mod popup;
pub use popup::PopupMenuFuture;
mod file_dialog;
mod message_box;
pub use dialogs::Dialogs;
pub use message_box::{MessageBoxFuture, MessageButtons, MessageResult};
mod property_sheet;
pub use file_dialog::{FileDialogFuture, FileDialogMode, FileFilter};
pub use property_sheet::{
    ApplyToken, PropertySheet, ValidatePage, ValidationError, ValidationToken,
};
mod connection;
mod events;
mod readiness;
mod tasks;
mod toplevel;
mod window;
pub use background::{Background, Blocking};
use classic_ui_abi::{self as abi, event};
pub use classic_ui_abi::{Date, Layout, Range, Rect, state};
pub use clipboard::ClipboardRead;
use connection::Connection;
pub use connection::{Error, Result};
pub use events::{EventStream, NextEvent};
pub use readiness::{AsyncFd, Readiness};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    future::Future,
    os::{fd::AsRawFd, unix::net::UnixStream},
    rc::Rc,
    sync::Arc,
    time::Duration,
};
pub use tasks::{AbortHandle, Cancelled, Clock, Sleep, Task};
pub use toplevel::{Toplevel, ToplevelRequest};
pub use window::{
    ColorRole, Column, ColumnAlignment, Draw, EditCommand, FontRole, Icon, ListStyle, Menu,
    MenuItem, RowIcon, ToolbarButton, ToolbarKind, TreeRow, WeakWindow, Widget, WidgetKind,
    WidgetSpec, Window, WindowOptions,
};

/// 按应用业务处理的语义事件；不公开 Rust/FFI 对象。
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum EventKind {
    Dismiss,
    Expand {
        row: u64,
        expanded: bool,
    },
    LabelEdited {
        row: u64,
    },
    Close,
    /// 连接上的标准顶层窗口快照发生变化；window/node 为零。
    ToplevelsChanged,
    Activate {
        value: u64,
    },
    Changed {
        value: i64,
    },
    Select {
        row: u64,
    },
    Key {
        code: u32,
        pressed: bool,
        repeat: bool,
        modifiers: u32,
        text: Option<char>,
        handled: bool,
    },
    Pointer {
        x: f64,
        y: f64,
    },
    Button {
        code: u32,
        pressed: bool,
        serial: u32,
    },
    Wheel {
        vertical: i32,
        horizontal: i32,
    },
    Configure {
        width: i32,
        height: i32,
        scale: i32,
    },
    Focus(bool),
    DialogDone {
        accepted: bool,
    },
    Column {
        column: u64,
    },
    ClipboardError {
        reason: u32,
    },
    Validate {
        token: ValidationToken,
    },
    Apply {
        token: ApplyToken,
    },
    /// 工具栏下拉箭头按下：`button` 为按钮标识，`rect` 为按钮在窗口客户区的逻辑矩形，菜单应放在其下方。
    Dropdown {
        button: u64,
        rect: Rect,
    },
}
/// 带稳定窗口与控件标识的事件，未知 ABI 事件自动跳过。
#[derive(Clone, Debug)]
pub struct Event {
    pub window: u64,
    pub node: u64,
    pub kind: EventKind,
}
impl Event {
    fn decode(event: abi::Event) -> Option<Self> {
        let kind = match event.kind {
            event::SHEET_VALIDATE => EventKind::Validate {
                token: ValidationToken(event.value),
            },
            event::SHEET_APPLY => EventKind::Apply {
                token: ApplyToken(event.value),
            },
            event::CLIPBOARD_ERROR => EventKind::ClipboardError { reason: event.code },
            event::DROPDOWN => EventKind::Dropdown {
                button: event.value,
                rect: Rect::new(event.x as i32, event.y as i32, event.width, event.height),
            },
            event::COLUMN => EventKind::Column {
                column: event.value,
            },
            event::DIALOG_DONE => EventKind::DialogDone {
                accepted: event.flags != 0,
            },
            event::CLOSE => EventKind::Close,
            event::DISMISS => EventKind::Dismiss,
            event::EXPAND => EventKind::Expand {
                row: event.value,
                expanded: event.flags != 0,
            },
            event::LABEL_EDITED => EventKind::LabelEdited { row: event.value },
            event::TOPLEVELS_CHANGED => EventKind::ToplevelsChanged,
            event::ACTIVATE => EventKind::Activate { value: event.value },
            event::CHANGED => EventKind::Changed {
                value: event.value as i64,
            },
            event::SELECT => EventKind::Select { row: event.value },
            event::KEY => EventKind::Key {
                handled: event.flags & 4 != 0,
                code: event.code,
                pressed: event.flags & 1 != 0,
                repeat: event.flags & 2 != 0,
                modifiers: event.modifiers,
                text: char::from_u32(event.text).filter(|c| *c != '\0'),
            },
            event::BUTTON => EventKind::Button {
                code: event.code,
                pressed: event.flags != 0,
                serial: event.value as u32,
            },
            event::WHEEL => EventKind::Wheel {
                vertical: event.width,
                horizontal: event.height,
            },
            event::POINTER => EventKind::Pointer {
                x: event.x,
                y: event.y,
            },
            event::CONFIGURE => EventKind::Configure {
                width: event.width,
                height: event.height,
                scale: event.scale,
            },
            event::FOCUS => EventKind::Focus(event.flags != 0),
            _ => return None,
        };
        Some(Self {
            window: event.window,
            node: event.node,
            kind,
        })
    }
}
/// 回调返回的事件循环动作；PreventDefault 可阻止默认关闭，以便异步询问保存。
pub enum ControlFlow {
    Continue,
    PreventDefault,
    Exit,
}

/// 一个 UI 线程的应用循环。异步任务、窗口与协议只通过本循环推进。
pub struct Application {
    connection: Rc<Connection>,
    windows: Rc<RefCell<BTreeMap<u64, WeakWindow>>>,
    executor: tasks::Executor,
    timers: Rc<tasks::Timers>,
    notify: Arc<tasks::Notifications>,
    reader: UnixStream,
    sources: BTreeMap<u64, std::rc::Weak<std::cell::RefCell<readiness::Ready>>>,
    background: Option<Background>,
    scope: Rc<tasks::Scope>,
    running: bool,
    subscriptions: Rc<RefCell<events::Subscriptions>>,
}
impl Application {
    /// 连接标准 Wayland compositor；缺少运行库、显示服务或唤醒资源时返回错误。
    pub fn new() -> Result<Self> {
        let connection = Connection::new()?;
        let (notify, reader) = tasks::Notifications::pair().map_err(io_error)?;
        Ok(Self {
            connection,
            windows: Rc::new(RefCell::new(BTreeMap::new())),
            executor: tasks::Executor::new(notify.clone()),
            timers: tasks::Timers::new(),
            notify,
            reader,
            sources: BTreeMap::new(),
            background: None,
            scope: Rc::new(tasks::Scope::new()),
            running: false,
            subscriptions: Rc::new(RefCell::new(events::Subscriptions::new())),
        })
    }
    /// 把非阻塞 FD 接入统一等待；返回的 AsyncFd 独占原描述符，析构时撤销注册。
    pub fn watch(&mut self, fd: std::os::fd::OwnedFd) -> Result<AsyncFd> {
        self.sources.retain(|_, source| source.strong_count() != 0);
        let source = AsyncFd::new(self.connection.clone(), fd)?;
        self.sources.insert(source.id, Rc::downgrade(&source.ready));
        Ok(source)
    }
    /// 异步读取 UTF-8 剪贴板；无文本返回 None，超时、超限或无效 UTF-8 返回错误。
    pub fn clipboard_text(&mut self, timeout: Duration) -> Result<ClipboardRead> {
        use std::os::fd::AsFd;
        let stream = self.connection.clipboard()?;
        let source = match &stream {
            Some(stream) => {
                Some(self.watch(stream.as_fd().try_clone_to_owned().map_err(io_error)?)?)
            }
            None => None,
        };
        Ok(ClipboardRead::new(stream, source, self.sleep(timeout)?))
    }
    /// 提供 UTF-8 文本选择；需要先收到有效的键盘或指针序号，容量上限 1 MiB。
    pub fn set_clipboard(&self, value: &str) -> Result<()> {
        // SAFETY: 文本在同步调用内有效，运行库复制后持有，无跨调用 Rust 借用。
        self.connection.check(unsafe {
            (self.connection.api.clipboard_set)(
                self.connection.context.as_ptr(),
                connection::text(value),
            )
        })
    }
    /// 订阅整个窗口的事件；关闭窗口终止流，参数必须属于本 Application。
    pub fn window_events(&mut self, window: &Window) -> Result<EventStream> {
        window.inner.check()?;
        if !Rc::ptr_eq(&self.connection, &window.inner.connection) {
            return Err(Error {
                code: abi::INVALID,
                message: "window belongs to another application".into(),
            });
        }
        self.subscriptions
            .borrow_mut()
            .subscribe(window.id(), None, &window.inner.scope)
    }
    /// 订阅一个控件的动作；交互状态仍由共享运行库独占。
    pub fn events(&mut self, widget: &Widget) -> Result<EventStream> {
        let window = widget.window();
        window.inner.check()?;
        if !Rc::ptr_eq(&self.connection, &window.inner.connection) {
            return Err(Error {
                code: abi::INVALID,
                message: "widget belongs to another application".into(),
            });
        }
        self.subscriptions
            .borrow_mut()
            .subscribe(window.id(), Some(widget.id()), widget.scope())
    }
    /// 打开非阻塞文件对话框并返回可 await 的结果；Drop Future 关闭子窗口。
    /// parent 必须属于本应用；路径保留 Linux 原生字节，不强制 UTF-8。
    pub fn file_dialog(
        &mut self,
        parent: &Window,
        mode: FileDialogMode,
        directory: &std::path::Path,
        name: &str,
    ) -> Result<FileDialogFuture> {
        self.dialogs().file(parent, mode, directory, name)
    }
    /// 打开带类型筛选的文件对话框；非法扩展名或不属于本应用的父窗口返回错误。
    /// 参数在返回前复制，结果和取消语义与 file_dialog 相同。
    pub fn file_dialog_filtered(
        &mut self,
        parent: &Window,
        mode: FileDialogMode,
        directory: &std::path::Path,
        name: &str,
        filter: FileFilter<'_>,
    ) -> Result<FileDialogFuture> {
        self.dialogs()
            .file_filtered(parent, mode, directory, name, filter)
    }
    /// 获取 UI 线程对话框入口，可在 async 任务中连续打开对话框；不会创建第二个事件循环。
    pub fn dialogs(&self) -> Dialogs {
        Dialogs {
            connection: self.connection.clone(),
            windows: self.windows.clone(),
            subscriptions: self.subscriptions.clone(),
        }
    }
    /// 创建窗口并加入应用路由；应用与窗口使用同一个稳定运行库连接。
    pub fn window(&mut self, options: WindowOptions<'_>) -> Result<Window> {
        let window = Window::new(self.connection.clone(), options)?;
        self.windows
            .borrow_mut()
            .insert(window.id(), window.downgrade());
        Ok(window)
    }
    /// 派发一次：先运行有界的就绪任务，再等待协议、任务唤醒或 deadline。
    /// timeout=None 可无限等待；超时/内部事件返回 None，断连与协议错误返回 Err。
    pub fn dispatch(&mut self, timeout: Option<Duration>) -> Result<Option<Event>> {
        tasks::Notifications::drain(&mut self.reader).map_err(io_error)?;
        self.timers.fire();
        self.executor.poll();
        // Future 可在轮询中关闭最后一个窗口；必须在进入无期限 poll 前把退出交还 run。
        if self.running
            && !self
                .windows
                .borrow()
                .values()
                .any(|window| window.upgrade().is_some())
        {
            return Ok(None);
        }
        let timeout = timeout.into_iter().chain(self.timers.next()).min();
        let timeout = if self.notify.ready() {
            Some(Duration::ZERO)
        } else {
            timeout
        };
        let milliseconds = timeout.map_or(-1, |duration| {
            duration
                .as_millis()
                .saturating_add(u128::from(duration.subsec_nanos() % 1_000_000 != 0))
                .min(i32::MAX as u128) as i32
        });
        let mut event = abi::Event::default();
        // SAFETY: Connection 和借用唤醒 fd 在调用中存活，输出事件独占，调用不进入应用回调。
        let code = unsafe {
            (self.connection.api.step)(
                self.connection.context.as_ptr(),
                milliseconds,
                self.reader.as_raw_fd(),
                &mut event,
            )
        };
        if code == abi::PENDING {
            return Ok(None);
        }
        self.connection.check(code)?;
        if event.kind == event::FD_READY {
            if let Some(ready) = self
                .sources
                .get(&event.value)
                .and_then(std::rc::Weak::upgrade)
            {
                let wake = {
                    let mut ready = ready.borrow_mut();
                    ready.flags = event.flags;
                    ready.armed = 0;
                    ready.waker.take()
                };
                if let Some(waker) = wake {
                    waker.wake();
                }
            }
            return Ok(None);
        }
        let event = Event::decode(event);
        if let Some(event) = &event {
            self.subscriptions.borrow_mut().publish(event);
        }
        Ok(event)
    }
    /// 运行到所有窗口关闭或回调返回 Exit。回调内创建/关闭窗口安全；嵌套运行返回错误。
    pub fn run(
        &mut self,
        mut handler: impl FnMut(&mut Self, Event) -> Result<ControlFlow>,
    ) -> Result<()> {
        if self.running {
            return Err(Error {
                code: abi::INVALID,
                message: "nested UI event loop is forbidden".into(),
            });
        }
        self.running = true;
        let result = (|| {
            loop {
                self.windows
                    .borrow_mut()
                    .retain(|_, window| window.upgrade().is_some());
                if self.windows.borrow().is_empty() {
                    return Ok(());
                }
                let Some(event) = self.dispatch(None)? else {
                    continue;
                };
                let close = matches!(event.kind, EventKind::Close);
                let id = event.window;
                match handler(self, event)? {
                    ControlFlow::Exit => return Ok(()),
                    ControlFlow::Continue if close => {
                        if let Some(window) =
                            self.windows.borrow().get(&id).and_then(WeakWindow::upgrade)
                        {
                            window.close()?;
                        }
                    }
                    _ => {}
                }
            }
        })();
        self.running = false;
        self.executor.clear();
        result
    }
    /// 在 UI 线程轮询非 Send Future，作用域随窗口关闭取消；必须保留返回的 Task owner。
    pub fn spawn_local<T: 'static>(
        &mut self,
        window: &Window,
        future: impl Future<Output = T> + 'static,
    ) -> Result<Task<T>> {
        if !Rc::ptr_eq(&self.connection, &window.inner.connection) {
            return Err(Error {
                code: abi::INVALID,
                message: "window belongs to another application".into(),
            });
        }
        window.inner.check()?;
        self.executor.spawn(window.inner.scope.clone(), future)
    }
    /// 创建应用作用域任务，用于没有特定窗口的异步工作；Drop Task 或应用退出时取消。
    pub fn spawn<T: 'static>(
        &mut self,
        future: impl Future<Output = T> + 'static,
    ) -> Result<Task<T>> {
        self.executor.spawn(self.scope.clone(), future)
    }
    /// 返回可在异步任务中创建后续等待的时钟；仍由本 Application 的事件循环推进。
    pub fn clock(&self) -> Clock {
        Clock {
            timers: self.timers.clone(),
        }
    }
    /// 等待单调时钟期限，不建立线程；Drop Future 取消定时器。
    pub fn sleep(&self, duration: Duration) -> Result<Sleep> {
        self.timers.sleep(duration)
    }
    /// 向有界后台池提交阻塞 I/O 或计算；队列满、线程创建失败立即返回错误。
    pub fn blocking<T: Send + 'static>(
        &mut self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<Blocking<T>> {
        self.background()?.run(work)
    }
    /// 返回同一个有界阻塞池的句柄，允许 async 任务在 await 之后继续提交工作。
    pub fn background(&mut self) -> Result<Background> {
        if self.background.is_none() {
            self.background = Some(Background::new()?);
        }
        Ok(self.background.as_ref().expect("initialized pool").clone())
    }
}
impl Drop for Application {
    fn drop(&mut self) {
        self.scope.cancel();
        self.executor.clear();
        for window in self
            .windows
            .borrow()
            .values()
            .filter_map(WeakWindow::upgrade)
        {
            let _ = window.close();
        }
    }
}
fn io_error(error: std::io::Error) -> Error {
    Error {
        code: abi::FAILED,
        message: error.to_string(),
    }
}
