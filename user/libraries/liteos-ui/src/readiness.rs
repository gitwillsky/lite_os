//! 一次性 FD readiness；与共享运行库的协议循环一起等待，无轮询线程。
use crate::{Result, connection::Connection};
use std::{
    cell::RefCell,
    future::Future,
    os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
    pin::Pin,
    rc::Rc,
    task::{Context, Poll, Waker},
};

pub(crate) struct Ready {
    pub flags: u32,
    pub waker: Option<Waker>,
    pub armed: u32,
}
/// 一个非阻塞 FD 的就绪注册；保留原 FD，运行库拥有独立 CLOEXEC 副本。
///
/// fd 的读写操作必须非阻塞。就绪只表示可以尝试操作，仍需处理 WouldBlock、EOF 和错误。
pub struct AsyncFd {
    connection: Rc<Connection>,
    fd: OwnedFd,
    pub(crate) id: u64,
    pub(crate) ready: Rc<RefCell<Ready>>,
}
impl AsyncFd {
    pub(crate) fn new(connection: Rc<Connection>, fd: OwnedFd) -> Result<Self> {
        let mut id = 0;
        // SAFETY: OwnedFd 在调用期间存活，运行库复制它，输出位置独占。
        connection.check(unsafe {
            (connection.api.watch)(connection.context.as_ptr(), fd.as_raw_fd(), &mut id)
        })?;
        Ok(Self {
            connection,
            fd,
            id,
            ready: Rc::new(RefCell::new(Ready {
                flags: 0,
                waker: None,
                armed: 0,
            })),
        })
    }
    pub(crate) fn poll_ready(&mut self, interest: u32, cx: &mut Context<'_>) -> Poll<Result<u32>> {
        let flags = self.ready.borrow().flags;
        if flags & (interest | 12) != 0 {
            self.ready.borrow_mut().flags = 0;
            return Poll::Ready(Ok(flags));
        }
        self.ready.borrow_mut().waker = Some(cx.waker().clone());
        if self.ready.borrow().armed != interest {
            // SAFETY: 注册由本句柄拥有，调用不会触发应用回调。
            if let Err(error) = self.connection.check(unsafe {
                (self.connection.api.arm)(self.connection.context.as_ptr(), self.id, interest)
            }) {
                return Poll::Ready(Err(error));
            }
            self.ready.borrow_mut().armed = interest;
        }
        Poll::Pending
    }
    /// 等待可读、错误或挂断；取消 Future 会撤销当前 interest。
    pub fn readable(&mut self) -> Readiness<'_> {
        Readiness {
            source: self,
            interest: 1,
            armed: false,
            complete: false,
        }
    }
    /// 等待可写、错误或挂断；同一注册只能同时拥有一个可变 readiness Future。
    pub fn writable(&mut self) -> Readiness<'_> {
        Readiness {
            source: self,
            interest: 2,
            armed: false,
            complete: false,
        }
    }
}
impl AsFd for AsyncFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}
impl Drop for AsyncFd {
    fn drop(&mut self) {
        // SAFETY: 注册由此 owner 释放一次，Connection 存活至本次同步调用返回。
        let _ = unsafe { (self.connection.api.unwatch)(self.connection.context.as_ptr(), self.id) };
    }
}
/// 返回就绪位 READ=1、WRITE=2、ERROR=4、HANGUP=8；不将 readiness 当成 I/O 完成。
pub struct Readiness<'a> {
    source: &'a mut AsyncFd,
    interest: u32,
    armed: bool,
    complete: bool,
}
impl Future for Readiness<'_> {
    type Output = Result<u32>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let interest = self.interest;
        let result = self.source.poll_ready(interest, cx);
        self.armed = true;
        if result.is_ready() {
            self.complete = true;
        }
        result
    }
}
impl Drop for Readiness<'_> {
    fn drop(&mut self) {
        self.source.ready.borrow_mut().waker = None;
        if self.armed && !self.complete {
            let c = &self.source.connection;
            // SAFETY: 同步撤销 interest 后没有任何回调引用此 Future；迟到事件只更新独立 Ready 状态。
            let _ = unsafe { (c.api.arm)(c.context.as_ptr(), self.source.id, 0) };
            self.source.ready.borrow_mut().armed = 0;
        }
    }
}
