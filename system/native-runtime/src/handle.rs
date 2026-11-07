//! 句柄所有权与所有对象共有的操作。

use native_abi::{HandleBasic, Status, call};

use crate::syscall::{check, invoke};

/// 独占拥有的句柄；离开作用域时关闭。
#[derive(Debug)]
pub struct Handle(native_abi::Handle);

impl Handle {
    /// 接管一个原始句柄值。
    ///
    /// # Safety
    ///
    /// `raw` 必须是调用者独占拥有、尚未被其他 `Handle` 接管的句柄（或 `HANDLE_INVALID`），
    /// 否则会重复关闭。
    pub const unsafe fn from_raw(raw: native_abi::Handle) -> Self {
        Self(raw)
    }

    /// 无效句柄。
    pub const fn invalid() -> Self {
        Self(native_abi::HANDLE_INVALID)
    }

    /// 是否为无效句柄。
    pub fn is_invalid(&self) -> bool {
        self.0 == native_abi::HANDLE_INVALID
    }

    /// 原始值；所有权保持不变。
    pub fn raw(&self) -> native_abi::Handle {
        self.0
    }

    /// 交出所有权，返回原始值。
    pub fn into_raw(self) -> native_abi::Handle {
        let raw = self.0;
        core::mem::forget(self);
        raw
    }

    /// 以不大于原句柄的 rights 复制。
    ///
    /// # Errors
    ///
    /// 缺少 `DUPLICATE` 或 rights 超出原句柄。
    pub fn duplicate(&self, rights: u32) -> Result<Self, Status> {
        let mut out = native_abi::HANDLE_INVALID;
        // SAFETY: `out` 是有效的输出位置。
        check(unsafe {
            invoke(
                call::HANDLE_DUPLICATE,
                &[self.0 as usize, rights as usize, (&raw mut out) as usize],
            )
        })?;
        Ok(Self(out))
    }

    /// 以不大于原句柄的 rights 替换；原句柄被消费。
    ///
    /// # Errors
    ///
    /// rights 超出原句柄；失败时原句柄同样已关闭（与内核语义一致）。
    pub fn replace(self, rights: u32) -> Result<Self, Status> {
        let mut out = native_abi::HANDLE_INVALID;
        let raw = self.into_raw();
        // SAFETY: `out` 是有效的输出位置。
        check(unsafe {
            invoke(
                call::HANDLE_REPLACE,
                &[raw as usize, rights as usize, (&raw mut out) as usize],
            )
        })?;
        Ok(Self(out))
    }

    /// 等待任一 `signals` 满足，返回观察到的 signal。
    ///
    /// # Errors
    ///
    /// `ERR_TIMED_OUT`（截止时间到，仍可能有部分观察值）、`ERR_CANCELED`（句柄被关闭）。
    pub fn wait_one(&self, signals: u32, deadline: i64) -> Result<u32, Status> {
        let mut observed = 0u32;
        // SAFETY: `observed` 是有效的输出位置。
        check(unsafe {
            invoke(
                call::OBJECT_WAIT_ONE,
                &[
                    self.0 as usize,
                    signals as usize,
                    deadline as usize,
                    (&raw mut observed) as usize,
                ],
            )
        })?;
        Ok(observed)
    }

    /// 注册一次性异步等待，满足时向 `port` 投递 `SIGNAL_ONE` packet。
    ///
    /// # Errors
    ///
    /// 对象不可等待或 port 缺少 `WRITE`。
    pub fn wait_async(&self, port: &Handle, key: u64, signals: u32) -> Result<(), Status> {
        // SAFETY: 无指针参数。
        check(unsafe {
            invoke(
                call::OBJECT_WAIT_ASYNC,
                &[
                    self.0 as usize,
                    port.0 as usize,
                    key as usize,
                    signals as usize,
                ],
            )
        })
    }

    /// 清除并置位用户 signal。
    ///
    /// # Errors
    ///
    /// 包含不可由用户修改的位。
    pub fn signal(&self, clear: u32, set: u32) -> Result<(), Status> {
        // SAFETY: 无指针参数。
        check(unsafe {
            invoke(
                call::OBJECT_SIGNAL,
                &[self.0 as usize, clear as usize, set as usize],
            )
        })
    }

    /// 读取 `topic` 信息到 `buffer`，返回实际字节数。
    ///
    /// # Errors
    ///
    /// `ERR_BUFFER_TOO_SMALL`、`ERR_NOT_SUPPORTED`。
    pub fn get_info(&self, topic: u32, buffer: &mut [u8]) -> Result<usize, Status> {
        let mut actual = 0usize;
        // SAFETY: 缓冲区与输出位置有效。
        check(unsafe {
            invoke(
                call::OBJECT_GET_INFO,
                &[
                    self.0 as usize,
                    topic as usize,
                    buffer.as_mut_ptr() as usize,
                    buffer.len(),
                    (&raw mut actual) as usize,
                ],
            )
        })?;
        Ok(actual)
    }

    /// 读取固定布局的信息结构。
    ///
    /// # Errors
    ///
    /// 同 [`Self::get_info`]；固定布局长度不符返回 `ERR_IO`，不能把缺失字段当作真实零值。
    pub fn info<T: crate::Plain>(&self, topic: u32) -> Result<T, Status> {
        let mut value = T::default();
        let actual = self.get_info(topic, crate::bytes_of_mut(&mut value))?;
        if actual != core::mem::size_of::<T>() {
            return Err(native_abi::ERR_IO);
        }
        Ok(value)
    }

    /// 读取变长 POD 数组；所需大小随对象变化时按内核回报的字节数扩容重试。
    pub(crate) fn info_array<T: crate::Plain>(
        &self,
        topic: u32,
    ) -> Result<alloc::vec::Vec<T>, Status> {
        let mut values = alloc::vec::Vec::<T>::new();
        let element_size = core::mem::size_of::<T>();
        if element_size == 0 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        loop {
            let mut actual = 0usize;
            // SAFETY: Vec 拥有 capacity 个 T 的可写空间，Plain 允许内核填入任意位模式。
            let status = unsafe {
                invoke(
                    call::OBJECT_GET_INFO,
                    &[
                        self.raw() as usize,
                        topic as usize,
                        values.as_mut_ptr() as usize,
                        values.capacity() * element_size,
                        (&raw mut actual) as usize,
                    ],
                )
            };
            match check(status) {
                Ok(()) => {
                    if !actual.is_multiple_of(element_size)
                        || actual > values.capacity() * element_size
                    {
                        return Err(native_abi::ERR_IO);
                    }
                    // SAFETY: 对应数组 topic 写入完整 T，actual 不超过传入容量；没有读取未初始化尾部。
                    unsafe { values.set_len(actual / element_size) };
                    return Ok(values);
                }
                Err(native_abi::ERR_BUFFER_TOO_SMALL) => {
                    if !actual.is_multiple_of(element_size)
                        || actual <= values.capacity() * element_size
                    {
                        return Err(native_abi::ERR_IO);
                    }
                    values
                        .try_reserve_exact(actual / element_size)
                        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                }
                Err(status) => return Err(status),
            }
        }
    }

    /// 对象类型、rights 与全局编号。
    ///
    /// # Errors
    ///
    /// 句柄无效。
    pub fn basic(&self) -> Result<HandleBasic, Status> {
        self.info(native_abi::info::HANDLE_BASIC)
    }

    /// 设置对象名称（≤ 32 字节）。
    ///
    /// # Errors
    ///
    /// 对象不支持名称或缺少 `MANAGE`。
    pub fn set_name(&self, name: &[u8]) -> Result<(), Status> {
        // SAFETY: 名称缓冲区有效。
        check(unsafe {
            invoke(
                call::OBJECT_SET_NAME,
                &[self.0 as usize, name.as_ptr() as usize, name.len()],
            )
        })
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if self.0 != native_abi::HANDLE_INVALID {
            // SAFETY: 句柄由本对象独占拥有；关闭失败只可能是句柄已无效，没有可恢复动作。
            let _ = unsafe { invoke(call::HANDLE_CLOSE, &[self.0 as usize]) };
        }
    }
}

/// 为类型化对象定义句柄包装：独占拥有、可解引用为 [`Handle`]。
macro_rules! object {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug)]
        pub struct $name($crate::Handle);

        impl $name {
            /// 把通用句柄视为该类型；类型不符时之后的调用返回 `ERR_WRONG_TYPE`。
            pub fn from_handle(handle: $crate::Handle) -> Self {
                Self(handle)
            }

            /// 交出通用句柄。
            pub fn into_handle(self) -> $crate::Handle {
                self.0
            }

            /// 以不大于原句柄的 rights 复制。
            ///
            /// # Errors
            ///
            /// 见 [`$crate::Handle::duplicate`]。
            pub fn duplicate(&self, rights: u32) -> Result<Self, native_abi::Status> {
                self.0.duplicate(rights).map(Self)
            }
        }

        impl core::ops::Deref for $name {
            type Target = $crate::Handle;

            fn deref(&self) -> &$crate::Handle {
                &self.0
            }
        }
    };
}

pub(crate) use object;
