//! PTY 输入队列有界且只在 UI 线程使用；空队列睡眠，可写等待接入 Application 的同一循环。
use std::{
    cell::RefCell,
    collections::VecDeque,
    future::poll_fn,
    rc::Rc,
    task::{Poll, Waker},
};
const LIMIT: usize = 1 << 20;
struct Pending {
    chunks: VecDeque<Vec<u8>>,
    bytes: usize,
    offset: usize,
    waiter: Option<Waker>,
}
#[derive(Clone)]
pub struct Input(Rc<RefCell<Pending>>);
impl Input {
    pub fn new() -> Self {
        Self(Rc::new(RefCell::new(Pending {
            chunks: VecDeque::new(),
            bytes: 0,
            offset: 0,
            waiter: None,
        })))
    }
    pub fn push(&self, bytes: Vec<u8>) -> std::io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let wake = {
            let mut queue = self.0.borrow_mut();
            if bytes.len() > LIMIT - queue.bytes {
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            queue.bytes += bytes.len();
            queue.chunks.push_back(bytes);
            queue.waiter.take()
        };
        if let Some(wake) = wake {
            wake.wake();
        }
        Ok(())
    }
    pub async fn pending(&self) {
        poll_fn(|cx| {
            let mut queue = self.0.borrow_mut();
            if queue.bytes != 0 {
                Poll::Ready(())
            } else {
                queue.waiter = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await
    }
    pub fn write(&self, pty: &mut linux_uapi::pty::PtySession) -> std::io::Result<usize> {
        let mut queue = self.0.borrow_mut();
        let Some(chunk) = queue.chunks.front() else {
            return Ok(0);
        };
        let count = pty.write(&chunk[queue.offset..])?;
        if count == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        let complete = queue.offset + count == chunk.len();
        queue.offset += count;
        queue.bytes -= count;
        if complete {
            queue.chunks.pop_front();
            queue.offset = 0;
        }
        Ok(count)
    }
}
