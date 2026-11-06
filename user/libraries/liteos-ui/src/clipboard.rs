//! 剪贴板 Future 等待 EOF 才发布完整文本；超时/取消不会返回截断的成功结果。
use crate::{
    AsyncFd, Clock, Error, Result, Sleep,
    connection::{Connection, text},
};
use std::{
    future::Future,
    io::{self, Read},
    os::{fd::AsFd, unix::net::UnixStream},
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};
const LIMIT: usize = 1 << 20;

/// 由 Application 推进的 MIME 剪贴板。读取复用异步 FD 与定时器。
pub struct Clipboard {
    connection: Rc<Connection>,
    clock: Clock,
}

impl Clipboard {
    pub(crate) fn new(connection: Rc<Connection>, clock: Clock) -> Self {
        Self { connection, clock }
    }

    /// 返回 `mime` 的选择身份；无该格式返回 None，运行库调用失败返回错误。
    pub fn revision(&self, mime: &str) -> Result<Option<u64>> {
        let mut revision = 0;
        let c = &self.connection;
        // SAFETY: MIME 与独占输出只在本次调用期间借用。
        c.check(unsafe {
            (c.api.clipboard_revision)(c.context.as_ptr(), text(mime), &mut revision)
        })?;
        Ok((revision != 0).then_some(revision))
    }

    /// 异步读取指定 MIME 的 UTF-8 数据；None 表示选首个纯文本格式。
    /// 无匹配格式返回 None，超时、超限或无效 UTF-8 返回错误。
    pub fn read(&self, mime: Option<&str>, timeout: std::time::Duration) -> Result<ClipboardRead> {
        let stream = self.connection.clipboard(mime)?;
        let source = stream
            .as_ref()
            .map(|stream| {
                self.connection.watch(
                    stream
                        .as_fd()
                        .try_clone_to_owned()
                        .map_err(crate::io_error)?,
                )
            })
            .transpose()?;
        Ok(ClipboardRead::new(
            stream,
            source,
            self.clock.sleep(timeout)?,
        ))
    }

    /// 发布 `formats`；空数组清空选择。指定 `expected` 时，仅替换仍在焦点内的同一次选择。
    /// 返回是否已发送请求，格式或容量非法及运行库调用失败返回错误。
    pub fn offer(&self, formats: &[(&str, &str)], expected: Option<u64>) -> Result<bool> {
        if formats.len() > 8 || expected == Some(0) {
            return Err(Error::invalid("invalid clipboard offer"));
        }
        let formats: Vec<_> = formats
            .iter()
            .map(|(mime, value)| classic_ui_abi::ClipboardFormat {
                mime: text(mime),
                value: text(value),
            })
            .collect();
        let c = &self.connection;
        // SAFETY: 格式数组与其借用的数据在同步调用期间有效，运行库在返回前复制。
        let code = unsafe {
            (c.api.clipboard_offer)(
                c.context.as_ptr(),
                formats.as_ptr(),
                formats.len(),
                expected.unwrap_or(0),
            )
        };
        if code == classic_ui_abi::PENDING {
            return Ok(false);
        }
        c.check(code)?;
        Ok(true)
    }
}

/// 异步 UTF-8 剪贴板传输。Drop 关闭接收端、撤销 FD interest 与定时器。
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
