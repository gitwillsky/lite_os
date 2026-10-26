//! 上下文菜单 Future；菜单渲染、焦点和按键由共享运行库处理。
use crate::{
    Dialogs, EventKind, EventStream, Menu, MenuItem, Result, Widget, WidgetKind, WidgetSpec, Window,
};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
/// 一次上下文菜单；取消或父窗口关闭返回 None，Drop 撤销菜单节点。
pub struct PopupMenuFuture {
    widget: Option<Widget>,
    events: EventStream,
    finished: bool,
}
impl Dialogs {
    /// 在父窗口当前指针处打开共享菜单；与菜单栏共用交互，不创建第二个 Wayland reader。
    pub fn menu(&self, parent: &Window, items: &[MenuItem<'_>]) -> Result<PopupMenuFuture> {
        self.open_menu(parent, items, None)
    }

    /// 把菜单左上角放在父窗口客户区逻辑坐标 `at`（如工具栏下拉按钮的左下角）。
    pub fn menu_at(
        &self,
        parent: &Window,
        items: &[MenuItem<'_>],
        at: (i32, i32),
    ) -> Result<PopupMenuFuture> {
        self.open_menu(parent, items, Some(at))
    }

    fn open_menu(
        &self,
        parent: &Window,
        items: &[MenuItem<'_>],
        at: Option<(i32, i32)>,
    ) -> Result<PopupMenuFuture> {
        self.check(parent)?;
        if items.is_empty() {
            return Err(crate::Error::invalid("context menu is empty"));
        }
        let widget = parent.widget(WidgetSpec::new(WidgetKind::PopupMenu, ""))?;
        widget.set_menus(&[Menu { title: "", items }])?;
        let events = self.subscriptions.borrow_mut().subscribe(
            parent.id(),
            Some(widget.id()),
            widget.scope(),
        )?;
        let (x, y) = at.unwrap_or((0, 0));
        // SAFETY: 句柄和连接均属于本 UI 线程；运行库已经复制菜单，不保留 Rust 对象。
        self.connection.check(unsafe {
            (self.connection.api.popup)(
                self.connection.context.as_ptr(),
                parent.id(),
                widget.id(),
                x,
                y,
                u32::from(at.is_some()),
            )
        })?;
        Ok(PopupMenuFuture {
            widget: Some(widget),
            events,
            finished: false,
        })
    }
}
impl Future for PopupMenuFuture {
    type Output = Result<Option<u64>>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        assert!(!self.finished, "PopupMenuFuture polled after completion");
        for _ in 0..16 {
            match self.events.poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => {
                    self.finished = true;
                    self.widget.take();
                    return Poll::Ready(Err(error));
                }
                Poll::Ready(Ok(None)) => {
                    self.finished = true;
                    self.widget.take();
                    return Poll::Ready(Ok(None));
                }
                Poll::Ready(Ok(Some(event))) => match event.kind {
                    EventKind::Activate { value } => {
                        self.finished = true;
                        self.widget.take();
                        return Poll::Ready(Ok(Some(value)));
                    }
                    EventKind::Dismiss => {
                        self.finished = true;
                        self.widget.take();
                        return Poll::Ready(Ok(None));
                    }
                    _ => {}
                },
            }
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}
