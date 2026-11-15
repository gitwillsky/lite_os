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
    /// Queues user input without closing the terminal when the PTY is backlogged.
    pub fn send(&self, bytes: Vec<u8>) -> bool {
        if let Err(error) = self.push(bytes) {
            eprintln!("terminal: input: {error}");
            return false;
        }
        true
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

/// Encodes clipboard text using xterm bracketed paste when the application enabled it.
pub fn paste(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let mut bytes = Vec::with_capacity(text.len() + 12);
        bytes.extend_from_slice(b"\x1b[200~");
        // A pasted ESC cannot end the bracketed-paste payload or turn it into terminal input.
        bytes.extend(text.bytes().filter(|byte| !matches!(byte, 0x1b | 0x03)));
        bytes.extend_from_slice(b"\x1b[201~");
        bytes
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn bracketed_paste_preserves_multiline_text_and_owns_the_delimiters() {
        assert_eq!(
            super::paste("甲\n乙\x1b[201~", true),
            "\x1b[200~甲\n乙[201~\x1b[201~".as_bytes()
        );
        assert_eq!(super::paste("甲\r\n乙\n", false), "甲\r乙\r".as_bytes());
    }
}
