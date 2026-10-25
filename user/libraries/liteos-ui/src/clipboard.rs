//! 剪贴板 Future 等待 EOF 才发布完整文本；超时/取消不会返回截断的成功结果。
use crate::{AsyncFd, Error, Result, Sleep};
use std::{
    future::Future,
    io::{self, Read},
    os::unix::net::UnixStream,
    pin::Pin,
    task::{Context, Poll},
};
const LIMIT: usize = 1 << 20;
/// 异步纯文本传输。Drop 关闭接收端、撤销 FD interest 与定时器。
pub struct ClipboardRead {
    stream: Option<UnixStream>,
    source: Option<AsyncFd>,
    timer: Sleep,
    bytes: Vec<u8>,
    finished: bool,
}
impl ClipboardRead {
    pub(crate) fn new(stream: Option<UnixStream>, source: Option<AsyncFd>, timer: Sleep) -> Self {
        Self {
            stream,
            source,
            timer,
            bytes: Vec::new(),
            finished: false,
        }
    }
    fn finish(&mut self) {
        self.finished = true;
        self.stream = None;
        self.source = None;
    }
}
impl Future for ClipboardRead {
    type Output = Result<Option<String>>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.finished {
            return Poll::Ready(Err(Error {
                code: classic_ui_abi::INVALID,
                message: "clipboard Future already completed".into(),
            }));
        }
        if self.stream.is_none() {
            self.finish();
            return Poll::Ready(Ok(None));
        }
        if Pin::new(&mut self.timer).poll(cx).is_ready() {
            self.finish();
            return Poll::Ready(Err(Error {
                code: classic_ui_abi::FAILED,
                message: "clipboard transfer timed out".into(),
            }));
        }
        // 每次 poll 最多读取 64 KiB，再让出执行机会；恶意无限输出不能独占 UI 线程。
        let mut budget = 65536usize;
        loop {
            let mut bytes = [0; 4096];
            match self
                .stream
                .as_mut()
                .expect("active clipboard")
                .read(&mut bytes)
            {
                Ok(0) => {
                    self.finish();
                    let bytes = std::mem::take(&mut self.bytes);
                    return Poll::Ready(String::from_utf8(bytes).map(Some).map_err(|_| Error {
                        code: classic_ui_abi::INVALID,
                        message: "clipboard is not UTF-8".into(),
                    }));
                }
                Ok(count) => {
                    if self.bytes.len() + count > LIMIT {
                        self.finish();
                        return Poll::Ready(Err(Error {
                            code: classic_ui_abi::INVALID,
                            message: "clipboard exceeds 1 MiB".into(),
                        }));
                    }
                    self.bytes.extend_from_slice(&bytes[..count]);
                    budget = budget.saturating_sub(count);
                    if budget == 0 {
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => match self
                    .source
                    .as_mut()
                    .expect("clipboard registration")
                    .poll_ready(1, cx)
                {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(_)) => continue,
                    Poll::Ready(Err(error)) => {
                        self.finish();
                        return Poll::Ready(Err(error));
                    }
                },
                Err(error) => {
                    self.finish();
                    return Poll::Ready(Err(crate::io_error(error)));
                }
            }
        }
    }
}
