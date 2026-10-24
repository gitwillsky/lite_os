//! musl 1.2.6 的可重入密码散列验证；账户与 shadow 策略由 logon 拥有。

use std::io;

/// 将字符串现有分配内的明文擦除，然后清空；不改变容量或分配新的副本。
///
/// # Parameters
/// `text` 是调用者独占的 UTF-8 字符串。
pub fn erase(text: &mut String) {
    // SAFETY: 独占 Vec 的整个 capacity 都是可写分配；只写不读未初始化尾部，零也保持有效 UTF-8。
    // 包括编辑删除后 len 以外的旧字符；volatile 阻止死存储消除，随后立即清空有效长度。
    unsafe {
        let bytes = text.as_mut_vec();
        for index in 0..bytes.capacity() {
            std::ptr::write_volatile(bytes.as_mut_ptr().add(index), 0);
        }
    }
    text.clear();
}

/// 使用 musl crypt_r 比较密码与已有散列，不接受锁定值或旧 DES/MD5 散列；空 shadow 字段的策略由调用方拥有。
///
/// # Parameters
/// `password` 为输入明文，`hash` 为 shadow 中完整的 $5$ 或 $6$ 散列。
/// # Returns
/// 密码是否匹配；不在进程全局缓存明文或散列。
/// # Errors
/// 非 musl Linux、含 NUL 的参数或 crypt_r 失败时返回错误。
pub fn verify(password: &str, hash: &str) -> io::Result<bool> {
    if !(hash.starts_with("$5$") || hash.starts_with("$6$")) {
        return Ok(false);
    }
    #[cfg(all(target_os = "linux", target_env = "musl"))]
    {
        use crate::raw::{CryptData, crypt_r};
        use std::ffi::{CStr, CString};
        let salt = CString::new(hash).map_err(|_| io::ErrorKind::InvalidInput)?;
        let key = CString::new(password).map_err(|_| io::ErrorKind::InvalidInput)?;
        let mut data = CryptData {
            initialized: 0,
            buffer: [0; 256],
        };
        // SAFETY: NUL 结尾的输入在调用期间存活；data 满足 musl 布局且没有其他借用。
        let result = unsafe { crypt_r(key.as_ptr(), salt.as_ptr(), &mut data) };
        let matched = if result.is_null() {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: crypt_r 成功返回本次 data 内 NUL 结尾的结果，比较后不保留其借用。
            let actual = unsafe { CStr::from_ptr(result) }.to_bytes();
            let mut difference = actual.len() ^ hash.len();
            for (left, right) in actual.iter().zip(hash.as_bytes()) {
                difference |= usize::from(left ^ right);
            }
            Ok(difference == 0)
        };
        let mut bytes = key.into_bytes_with_nul();
        for byte in &mut bytes {
            // SAFETY: bytes 为本次调用独占分配，写入不改变指针或长度。
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
        matched
    }
    #[cfg(not(all(target_os = "linux", target_env = "musl")))]
    {
        let _ = password;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "password verification requires product musl",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn erase_clears_deleted_tail_and_preserves_the_allocation() {
        let mut text = String::with_capacity(64);
        // 先初始化所有容量，使旧实现未擦除的尾部也能被安全读取并确定性断言。
        text.push_str(&"x".repeat(text.capacity()));
        text.clear();
        text.push_str("deleted password");
        text.truncate(2);
        let capacity = text.capacity();
        let pointer = text.as_ptr();
        erase(&mut text);
        assert!(text.is_empty());
        assert_eq!(text.capacity(), capacity);
        assert_eq!(text.as_ptr(), pointer);
        // SAFETY: 本测试已初始化整个 capacity，独占 String 且只读仍存活的分配内字节。
        let cleared = unsafe { std::slice::from_raw_parts(text.as_ptr(), text.capacity()) };
        assert!(cleared.iter().all(|byte| *byte == 0));
    }
}
