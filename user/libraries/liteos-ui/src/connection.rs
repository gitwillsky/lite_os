//! 稳定 ABI 表的唯一导入入口，以及错误和输出缓冲编解码；Rust owner 不穿过二进制边界。
use classic_ui_abi::{self as abi, Api, Text};
use std::{
    ffi::c_void,
    fmt,
    marker::PhantomData,
    ptr::{self, NonNull},
    rc::Rc,
};

#[link(name = "liteos_ui")]
unsafe extern "C" {
    fn liteos_ui_get_api(version: u32) -> *const Api;
}

/// SDK 与运行库返回的错误；code 对应稳定 ABI 状态，message 在当前调用内复制。
#[derive(Debug, Clone)]
pub struct Error {
    pub code: i32,
    pub message: String,
}
impl Error {
    /// 创建业务参数校验错误，保留稳定 SDK 的 INVALID 错误码。
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: abi::INVALID,
            message: message.into(),
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
pub fn text(value: &str) -> Text {
    Text {
        data: value.as_ptr(),
        len: value.len(),
    }
}

pub struct Connection {
    pub api: &'static Api,
    pub context: NonNull<c_void>,
    // Rc marker 使所有拥有连接的句柄不能 Send/Sync；禁止在另一线程借用裸 context。
    _thread: PhantomData<Rc<()>>,
}
impl Connection {
    pub fn new() -> Result<Rc<Self>> {
        // SAFETY: 动态加载器已解析此固定符号，返回的是进程存活期内的只读 repr(C) 表。
        let pointer = unsafe { liteos_ui_get_api(abi::ABI_VERSION) };
        if pointer.is_null() {
            return Err(Error {
                code: abi::FAILED,
                message: "unsupported LiteOS UI ABI".into(),
            });
        }
        // SAFETY: 查询契约至少返回两个 u32 的头部；先验证长度，不能先构造完整 Api 引用。
        let header = unsafe { pointer.cast::<u32>().read() };
        let size = unsafe { pointer.cast::<u32>().add(1).read() };
        if header != abi::ABI_VERSION || (size as usize) < size_of::<Api>() {
            return Err(Error {
                code: abi::FAILED,
                message: "incomplete LiteOS UI ABI table".into(),
            });
        }
        let api = unsafe { &*pointer };
        let mut context = ptr::null_mut();
        // SAFETY: 输出位置独占有效，成功后 Connection 唯一拥有销毁权。
        let code = unsafe { (api.create)(&mut context) };
        let context = NonNull::new(context)
            .filter(|_| code == abi::OK)
            .ok_or_else(|| Error {
                code,
                message: "cannot create UI connection".into(),
            })?;
        Ok(Rc::new(Self {
            api,
            context,
            _thread: PhantomData,
        }))
    }
    pub fn clipboard(&self) -> Result<Option<std::os::unix::net::UnixStream>> {
        use std::os::fd::{FromRawFd, OwnedFd};
        let mut fd = -1;
        // SAFETY: context 存活，成功返回一个新 fd 的唯一所有权，本输出不与 context 重叠。
        let code = unsafe { (self.api.clipboard_get)(self.context.as_ptr(), &mut fd) };
        if code == abi::PENDING {
            return Ok(None);
        }
        self.check(code)?;
        if fd < 0 {
            return Err(Error {
                code: abi::INVALID,
                message: "invalid clipboard descriptor".into(),
            });
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Some(std::os::unix::net::UnixStream::from(fd)))
    }
    pub fn check(&self, code: i32) -> Result<()> {
        if code == abi::OK {
            return Ok(());
        }
        let message = self
            .read(|buffer, capacity, written| {
                // SAFETY: Connection 的生命周期包含本次调用，read 提供有效缓冲和长度输出。
                unsafe { (self.api.error_text)(self.context.as_ptr(), buffer, capacity, written) }
            })
            .unwrap_or_else(|_| format!("UI operation failed ({code})"));
        Err(Error { code, message })
    }
    pub fn read(&self, call: impl FnMut(*mut u8, usize, *mut usize) -> i32) -> Result<String> {
        String::from_utf8(self.read_bytes(call)?).map_err(|_| Error {
            code: abi::INVALID,
            message: "runtime returned invalid UTF-8".into(),
        })
    }
    pub fn read_bytes(
        &self,
        mut call: impl FnMut(*mut u8, usize, *mut usize) -> i32,
    ) -> Result<Vec<u8>> {
        let mut length = 0;
        let code = call(ptr::null_mut(), 0, &mut length);
        if code != abi::OK && code != abi::TOO_SMALL {
            return Err(Error {
                code,
                message: "UI text query failed".into(),
            });
        }
        if length > 16 * 1024 * 1024 {
            return Err(Error {
                code: abi::INVALID,
                message: "UI text is too large".into(),
            });
        }
        let mut bytes = vec![0; length];
        let mut written = 0;
        let code = call(bytes.as_mut_ptr(), bytes.len(), &mut written);
        if code != abi::OK || written > bytes.len() {
            return Err(Error {
                code,
                message: "UI text copy failed".into(),
            });
        }
        bytes.truncate(written);
        Ok(bytes)
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        // SAFETY: Rc 的最后一个 owner 在创建线程销毁，所有窗口/控件已先释放，不存在在途调用。
        unsafe {
            (self.api.destroy)(self.context.as_ptr());
        }
    }
}
