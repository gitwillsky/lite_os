//! 调用进程所属 UTS 命名空间的系统标识。

use std::{ffi::OsString, io};

/// `uname` 报告的主机名和系统发行版本。
pub struct Identity {
    /// 当前 UTS 命名空间的主机名。
    pub hostname: OsString,
    /// 内核或 Linux 兼容服务报告的发行版本。
    pub release: OsString,
}

/// 读取当前 UTS 标识。名称保留系统返回的原始字节。
///
/// # Errors
///
/// 系统调用失败，或目标平台不是 Linux。
pub fn read() -> io::Result<Identity> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStringExt;
        // Linux struct new_utsname 的 6 个字段各含 64 字节及终止 NUL。
        let mut fields = [[0u8; 65]; 6];
        // SAFETY: fields 可写且完整覆盖 Linux new_utsname，调用期间独占。
        if unsafe { crate::raw::uname(fields.as_mut_ptr().cast()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let text = |field: &[u8]| {
            OsString::from_vec(
                field
                    .split(|byte| *byte == 0)
                    .next()
                    .unwrap_or_default()
                    .to_vec(),
            )
        };
        Ok(Identity {
            hostname: text(&fields[1]),
            release: text(&fields[2]),
        })
    }
    #[cfg(not(target_os = "linux"))]
    Err(io::ErrorKind::Unsupported.into())
}
