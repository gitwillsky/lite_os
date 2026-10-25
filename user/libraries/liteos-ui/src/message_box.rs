//! 消息对话框的异步适配；布局与控件行为来自版本化运行库，不嵌套事件循环。
use crate::{Application, EventKind, EventStream, Result, Window};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
/// 标准消息按钮组合，首项为默认动作。
#[derive(Clone, Copy)]
#[repr(u32)]
pub enum MessageButtons {
    Ok,
    OkCancel,
    YesNoCancel,
    YesNo,
}
/// 用户选择；标题栏关闭和 Escape 都返回 Cancel。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageResult {
    Ok,
    Yes,
    No,
    Cancel,
}
/// 消息对话框完成；Drop 会关闭子窗口，父窗口关闭也取消等待。
pub struct MessageBoxFuture {
    window: Window,
    events: EventStream,
    buttons: [u64; 3],
    kind: MessageButtons,
    finished: bool,
}
impl MessageBoxFuture {
    pub(crate) fn new(
        window: Window,
        events: EventStream,
        buttons: [u64; 3],
        kind: MessageButtons,
    ) -> Self {
        Self {
            window,
            events,
            buttons,
            kind,
            finished: false,
        }
    }
}
impl Application {
    /// 打开通用模态消息框；参数须属于本应用，资源/协议失败返回 Err。
    pub fn message_box(
        &mut self,
        parent: &Window,
        title: &str,
        message: &str,
        buttons: MessageButtons,
    ) -> Result<MessageBoxFuture> {
        self.dialogs().message(parent, title, message, buttons)
    }
}
impl Future for MessageBoxFuture {
    type Output = Result<MessageResult>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        assert!(!self.finished, "MessageBoxFuture polled after completion");
        if self.window.inner.check().is_err() {
            self.finished = true;
            return Poll::Ready(Ok(MessageResult::Cancel));
        }
        for _ in 0..16 {
            let result = match self.events.poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => {
                    self.finished = true;
                    let _ = self.window.close();
                    return Poll::Ready(Err(error));
                }
                Poll::Ready(Ok(None)) => Some(MessageResult::Cancel),
                Poll::Ready(Ok(Some(event))) => match event.kind {
                    EventKind::Close
                    | EventKind::Key {
                        code: 1,
                        pressed: true,
                        ..
                    } => Some(MessageResult::Cancel),
                    EventKind::Activate { .. } => self
                        .buttons
                        .iter()
                        .position(|id| *id == event.node && *id != 0)
                        .map(|index| match (self.kind, index) {
                            (MessageButtons::Ok | MessageButtons::OkCancel, 0) => MessageResult::Ok,
                            (MessageButtons::YesNoCancel | MessageButtons::YesNo, 0) => {
                                MessageResult::Yes
                            }
                            (MessageButtons::YesNoCancel | MessageButtons::YesNo, 1) => {
                                MessageResult::No
                            }
                            _ => MessageResult::Cancel,
                        }),
                    _ => None,
                },
            };
            if let Some(result) = result {
                self.finished = true;
                let _ = self.window.close();
                return Poll::Ready(Ok(result));
            }
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}
impl Drop for MessageBoxFuture {
    fn drop(&mut self) {
        let _ = self.window.close();
    }
}
