//! 用户内存访问。
//!
//! 复制先按 VMA 校验 Linux 权限，再经内核 `process_read/write_memory` 完成：内核在目标地址空间中
//! 解决缺页，后备无法提供页时返回错误（对应 EFAULT），不会让服务线程自身故障。
//! futex 等原子操作要求当前服务线程运行在该地址空间中，校验后直接对用户地址执行。

use alloc::vec::Vec;
use core::sync::atomic::AtomicU32;

use super::MemorySet;
use crate::memory::{FutexKey, MapPermission, PAGE_SIZE, UserAccessError, UserAtomicU32};

/// 单次内核复制的上限；更长的复制分段进行，限制服务栈与临时缓冲。
const CHUNK: usize = 64 * 1024;

impl MemorySet {
    /// `[address, address + length)` 是否被具有 `permission` 的 VMA 连续覆盖。
    fn check_access(&self, address: usize, length: usize, permission: MapPermission) -> Result<(), UserAccessError> {
        if length == 0 {
            return Ok(());
        }
        let end = address.checked_add(length).ok_or(UserAccessError::Overflow)?;
        if end > self.trampoline() || address < self.base.max(PAGE_SIZE) {
            return Err(UserAccessError::Fault);
        }
        let mut cursor = address;
        while cursor < end {
            let area = self.area_at(cursor).ok_or(UserAccessError::Fault)?;
            if !area.permission.contains(permission) {
                return Err(UserAccessError::Fault);
            }
            cursor = area.end;
        }
        Ok(())
    }

    fn read_raw(&self, address: usize, output: &mut [u8]) -> Result<(), UserAccessError> {
        let mut done = 0;
        while done < output.len() {
            let chunk = (output.len() - done).min(CHUNK);
            let actual = self
                .process
                .read_memory(address + done, &mut output[done..done + chunk])
                .map_err(|_| UserAccessError::Fault)?;
            if actual == 0 {
                return Err(UserAccessError::Fault);
            }
            done += actual;
        }
        Ok(())
    }

    fn write_raw(&self, address: usize, input: &[u8]) -> Result<(), UserAccessError> {
        let mut done = 0;
        while done < input.len() {
            let chunk = (input.len() - done).min(CHUNK);
            let actual = self
                .process
                .write_memory(address + done, &input[done..done + chunk])
                .map_err(|_| UserAccessError::Fault)?;
            if actual == 0 {
                return Err(UserAccessError::Fault);
            }
            done += actual;
        }
        Ok(())
    }

    /// 从用户地址复制到 `output`。
    ///
    /// # Errors
    ///
    /// 区间不可读或溢出。
    pub(crate) fn copy_from_user(&self, address: usize, output: &mut [u8]) -> Result<(), UserAccessError> {
        self.check_access(address, output.len(), MapPermission::R)?;
        self.read_raw(address, output)
    }

    /// 从用户地址读取 `length` 字节到新缓冲。
    ///
    /// # Errors
    ///
    /// 区间不可读、溢出或内存不足。
    pub(crate) fn read_user_bytes(&self, address: usize, length: usize) -> Result<Vec<u8>, UserAccessError> {
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(length).map_err(|_| UserAccessError::OutOfMemory)?;
        bytes.resize(length, 0);
        self.copy_from_user(address, &mut bytes)?;
        Ok(bytes)
    }

    /// 把 `input` 复制到用户地址。
    ///
    /// # Errors
    ///
    /// 区间不可写或溢出。
    pub(crate) fn copy_to_user(&self, address: usize, input: &[u8]) -> Result<(), UserAccessError> {
        self.check_access(address, input.len(), MapPermission::W)?;
        self.write_raw(address, input)
    }

    /// 把用户区间清零。
    ///
    /// # Errors
    ///
    /// 区间不可写或溢出。
    pub(crate) fn zero_user(&self, address: usize, length: usize) -> Result<(), UserAccessError> {
        self.check_access(address, length, MapPermission::W)?;
        let zeros = [0u8; 512];
        let mut done = 0;
        while done < length {
            let chunk = (length - done).min(zeros.len());
            self.write_raw(address + done, &zeros[..chunk])?;
            done += chunk;
        }
        Ok(())
    }

    /// 校验用户区间可写（不访问内容）。
    ///
    /// # Errors
    ///
    /// 区间不可写或溢出。
    pub(crate) fn validate_user_write(&self, address: usize, length: usize) -> Result<(), UserAccessError> {
        self.check_access(address, length, MapPermission::W)
    }

    /// 读取以 NUL 结尾的用户字符串（不含 NUL），最多 `limit` 字节。
    ///
    /// # Errors
    ///
    /// 不可读（`Fault`）、超过上限仍未结束（`Unterminated`）、内存不足。
    pub(crate) fn copy_user_c_string(&self, address: usize, limit: usize) -> Result<Vec<u8>, UserAccessError> {
        let mut string = Vec::new();
        let mut cursor = address;
        let mut chunk = [0u8; 256];
        loop {
            // 每段不跨页：页之后可能未映射，而字符串在本页内已经结束。
            let page_remaining = PAGE_SIZE - (cursor % PAGE_SIZE);
            let length = chunk.len().min(page_remaining).min(limit.saturating_sub(string.len()).max(1));
            self.copy_from_user(cursor, &mut chunk[..length])?;
            if let Some(end) = chunk[..length].iter().position(|&byte| byte == 0) {
                string.try_reserve(end).map_err(|_| UserAccessError::OutOfMemory)?;
                string.extend_from_slice(&chunk[..end]);
                return Ok(string);
            }
            if string.len() + length > limit {
                return Err(UserAccessError::Unterminated);
            }
            string.try_reserve(length).map_err(|_| UserAccessError::OutOfMemory)?;
            string.extend_from_slice(&chunk[..length]);
            cursor = cursor.checked_add(length).ok_or(UserAccessError::Overflow)?;
            if string.len() >= limit {
                return Err(UserAccessError::Unterminated);
            }
        }
    }

    /// 读取用户 `u32`。
    ///
    /// # Errors
    ///
    /// 不可读或未对齐。
    pub(crate) fn load_user_u32(&self, address: usize) -> Result<u32, UserAccessError> {
        if !address.is_multiple_of(4) {
            return Err(UserAccessError::Fault);
        }
        let mut bytes = [0u8; 4];
        self.copy_from_user(address, &mut bytes)?;
        Ok(u32::from_ne_bytes(bytes))
    }

    /// 校验后的用户原子字。
    ///
    /// 调用者保证当前服务线程运行在本地址空间（该 Linux 进程的成员进程）中，且在返回值存活期间持有
    /// 本地址空间的锁：VMA 不会被并发解除，缺页与写时复制由内核在访问时解决。
    ///
    /// # Errors
    ///
    /// 未对齐、不可读或（`write` 时）不可写。
    pub(crate) fn user_atomic_u32(&self, address: usize, write: bool) -> Result<UserAtomicU32<'_>, UserAccessError> {
        if !address.is_multiple_of(4) {
            return Err(UserAccessError::Fault);
        }
        let permission = if write {
            MapPermission::R | MapPermission::W
        } else {
            MapPermission::R
        };
        self.check_access(address, 4, permission)?;
        // SAFETY: 地址对齐、落在具有所需权限的 VMA 内；VMA 由调用者持有的地址空间锁固定，
        // 当前线程运行在同一地址空间，因此该地址指向此 Linux 进程的内存。
        Ok(UserAtomicU32(unsafe { &*(address as *const AtomicU32) }))
    }

    /// 比较交换用户 `u32`。
    ///
    /// # Errors
    ///
    /// 同 [`Self::user_atomic_u32`]。
    pub(crate) fn compare_exchange_user_u32(&self, address: usize, current: u32, replacement: u32) -> Result<Result<u32, u32>, UserAccessError> {
        Ok(self.user_atomic_u32(address, true)?.compare_exchange(current, replacement))
    }

    /// 为 futex 地址生成等待身份。
    ///
    /// # Errors
    ///
    /// 未对齐、越界或（共享 key 时）不可读。
    pub(crate) fn futex_key(&self, address: usize, address_space: usize, private: bool) -> Result<FutexKey, UserAccessError> {
        if private {
            return FutexKey::private(address, address_space);
        }
        if !address.is_multiple_of(4) {
            return Err(UserAccessError::Fault);
        }
        self.check_access(address, 4, MapPermission::R)?;
        let area = self.area_at(address).ok_or(UserAccessError::Fault)?;
        if area.backing.is_shared() {
            return Ok(FutexKey::Shared {
                object: area.backing.object().koid(),
                offset: area.backing.offset() + (address - area.start) as u64,
            });
        }
        Ok(FutexKey::Private { address_space, address })
    }

    /// 调试访问（ptrace、`/proc/<pid>/mem`）：忽略 Linux 权限，只要求已映射；写只读私有映射时内核为本进程
    /// 复制该页（与 Linux `FOLL_FORCE` 相同）。返回实际传输字节数（遇到未映射处截断）。
    pub(crate) fn access_forced(&self, address: usize, buffer: &mut [u8], write: bool) -> usize {
        let mut done = 0;
        while done < buffer.len() {
            let Some(area) = self.area_at(address + done) else { break };
            let chunk = (buffer.len() - done).min(area.end - (address + done));
            let result = if write {
                self.write_raw(address + done, &buffer[done..done + chunk])
            } else {
                self.read_raw(address + done, &mut buffer[done..done + chunk])
            };
            if result.is_err() {
                break;
            }
            done += chunk;
        }
        done
    }

    /// 读取 argv 字符串（`/proc/<pid>/cmdline`）。
    ///
    /// # Errors
    ///
    /// 区间已不可读或内存不足。
    pub(crate) fn process_arguments(&self) -> Result<Vec<u8>, UserAccessError> {
        let range = self.argument_range();
        self.read_user_bytes(range.start, range.end - range.start)
    }
}
