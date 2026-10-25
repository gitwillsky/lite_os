//! 文件对话框结果 Future；共用同一个 UI 循环，不创建嵌套消息循环。
use crate::{EventKind, EventStream, Result, Window};
use std::{
    future::Future,
    os::unix::ffi::OsStringExt,
    path::PathBuf,
    pin::Pin,
    task::{Context, Poll},
};
/// 文件选择用途，已选路径的后续业务操作由应用完成。
#[derive(Clone, Copy, Debug)]
#[repr(u32)]
pub enum FileDialogMode {
    Open = 0,
    Save = 1,
}
/// 文件列表的类型筛选；扩展名不含前导点，按 ASCII 大小写不敏感匹配。
/// 对话框仍显示目录，并允许切换“所有文件”或手工输入其他类型的路径。
pub struct FileFilter<'a> {
    pub label: &'a str,
    pub extensions: &'a [&'a str],
}
/// 文件选择结果，用户取消或父窗口关闭返回 None；连接/读取错误返回 Err。
pub struct FileDialogFuture {
    window: Window,
    events: EventStream,
    finished: bool,
}
impl FileDialogFuture {
    pub(crate) fn new(window: Window, events: EventStream) -> Self {
        Self {
            window,
            events,
            finished: false,
        }
    }
}
impl Future for FileDialogFuture {
    type Output = Result<Option<PathBuf>>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.finished {
            panic!("FileDialogFuture polled after completion");
        }
        if self.window.inner.check().is_err() {
            self.finished = true;
            return Poll::Ready(Ok(None));
        }
        for _ in 0..16 {
            match self.events.poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => {
                    self.finished = true;
                    let _ = self.window.close();
                    return Poll::Ready(Err(error));
                }
                Poll::Ready(Ok(None)) => {
                    self.finished = true;
                    return Poll::Ready(Ok(None));
                }
                Poll::Ready(Ok(Some(event))) => {
                    if let EventKind::DialogDone { accepted } = event.kind {
                        let result = if accepted {
                            let c = &self.window.inner.connection;
                            c.read_bytes(|buffer, capacity, written| {
                                // SAFETY: 窗口、输出缓冲和 context 存活，结果复制后由应用独占。
                                unsafe {
                                    (c.api.dialog_result)(
                                        c.context.as_ptr(),
                                        self.window.id(),
                                        buffer,
                                        capacity,
                                        written,
                                    )
                                }
                            })
                            .map(|bytes| Some(PathBuf::from(std::ffi::OsString::from_vec(bytes))))
                        } else {
                            Ok(None)
                        };
                        self.finished = true;
                        let close = self.window.close();
                        return Poll::Ready(result.and_then(|result| close.map(|_| result)));
                    }
                }
            }
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}
impl Drop for FileDialogFuture {
    fn drop(&mut self) {
        let _ = self.window.close();
    }
}
