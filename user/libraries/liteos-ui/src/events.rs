//! 有界事件订阅；应用可顺序 await 控件动作，不需要嵌套事件循环。
use crate::{
    Error, Event, EventKind, Result,
    tasks::{OnCancel, Scope},
};
use std::{
    cell::RefCell,
    collections::VecDeque,
    future::Future,
    pin::Pin,
    rc::{Rc, Weak},
    task::{Context, Poll, Waker},
};
const CAPACITY: usize = 64;
struct State {
    events: VecDeque<Event>,
    closed: bool,
    overflow: bool,
    waker: Option<Waker>,
}
struct Inbox {
    window: u64,
    node: Option<u64>,
    state: RefCell<State>,
}
impl OnCancel for Inbox {
    fn cancel(&self) {
        let wake = {
            let mut state = self.state.borrow_mut();
            state.closed = true;
            state.events.clear();
            state.waker.take()
        };
        if let Some(waker) = wake {
            waker.wake();
        }
    }
}
pub(crate) struct Subscriptions {
    inboxes: Vec<Weak<Inbox>>,
}
impl Subscriptions {
    pub fn new() -> Self {
        Self {
            inboxes: Vec::new(),
        }
    }
    pub fn subscribe(
        &mut self,
        window: u64,
        node: Option<u64>,
        scope: &Scope,
    ) -> Result<EventStream> {
        self.inboxes.retain(|inbox| inbox.strong_count() != 0);
        if self.inboxes.len() >= 4096 {
            return Err(Error {
                code: classic_ui_abi::FAILED,
                message: "event subscription limit reached".into(),
            });
        }
        let inner = Rc::new(Inbox {
            window,
            node,
            state: RefCell::new(State {
                events: VecDeque::new(),
                closed: false,
                overflow: false,
                waker: None,
            }),
        });
        self.inboxes.retain(|inbox| inbox.strong_count() != 0);
        self.inboxes.push(Rc::downgrade(&inner));
        scope.observe(inner.clone());
        Ok(EventStream { inner })
    }
    pub fn publish(&mut self, event: &Event) {
        let mut wakes = Vec::new();
        self.inboxes.retain(|inbox| inbox.strong_count() != 0);
        for inbox in self
            .inboxes
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|inbox| {
                inbox.window == event.window && inbox.node.is_none_or(|node| node == event.node)
            })
        {
            let mut state = inbox.state.borrow_mut();
            if state.closed {
                continue;
            }
            let coalesce = state.events.back().is_some_and(|last| {
                last.node == event.node
                    && matches!(
                        (&last.kind, &event.kind),
                        (EventKind::Pointer { .. }, EventKind::Pointer { .. })
                            | (EventKind::Configure { .. }, EventKind::Configure { .. })
                    )
            });
            if coalesce {
                state.events.pop_back();
            }
            if state.events.len() == CAPACITY {
                state.overflow = true;
                state.closed = true;
                state.events.clear();
            } else {
                state.events.push_back(event.clone());
            }
            if let Some(waker) = state.waker.take() {
                wakes.push(waker);
            }
        }
        // 唤醒不在任何订阅借用内执行，允许正常 Future 调度而不产生 RefCell 重入。
        for waker in wakes {
            waker.wake();
        }
    }
}
/// 一个窗口或控件的事件流；关闭窗口返回 None，队列溢出明确返回错误。
pub struct EventStream {
    inner: Rc<Inbox>,
}
impl EventStream {
    /// 等待下一次事件；Drop 此 Future 取消本次等待，Drop EventStream 撤销订阅。
    pub fn next(&mut self) -> NextEvent<'_> {
        NextEvent { stream: self }
    }
}
/// 一次事件等待；多个等待必须顺序进行，不能并发借用同一事件流。
pub struct NextEvent<'a> {
    stream: &'a mut EventStream,
}
impl EventStream {
    pub(crate) fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<Event>>> {
        let mut state = self.inner.state.borrow_mut();
        if state.overflow {
            state.overflow = false;
            return Poll::Ready(Err(Error {
                code: classic_ui_abi::FAILED,
                message: "UI event subscription overflow".into(),
            }));
        }
        if let Some(event) = state.events.pop_front() {
            return Poll::Ready(Ok(Some(event)));
        }
        if state.closed {
            return Poll::Ready(Ok(None));
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}
impl Future for NextEvent<'_> {
    type Output = Result<Option<Event>>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().stream.poll_next(cx)
    }
}
impl Drop for NextEvent<'_> {
    fn drop(&mut self) {
        self.stream.inner.state.borrow_mut().waker = None;
    }
}
