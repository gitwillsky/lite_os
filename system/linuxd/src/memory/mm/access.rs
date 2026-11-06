//! 用户内存访问。
//!
//! 按 VMA 校验权限。同一成员的承载线程直接复制，缺页由 VM 解决，不可恢复的应用侧故障
//! 由已登记的复制例程返回 EFAULT。跨地址空间、调试访问和可执行映射写入走内核远程访问。

use alloc::vec::Vec;
use core::mem::MaybeUninit;
use core::sync::atomic::AtomicU32;

use super::MemorySet;
use crate::memory::{FutexKey, MapPermission, PAGE_SIZE, UserAccessError, UserAtomicU32};

/// 单次内核复制的上限；更长的复制分段进行，限制服务栈与临时缓冲。
const CHUNK: usize = 64 * 1024;

impl MemorySet {
    /// 校验区间的 VMA 连续性与权限，返回区间是否包含可执行映射。
    fn check_access(
        &self,
        address: usize,
        length: usize,
        permission: MapPermission,
    ) -> Result<bool, UserAccessError> {
        if length == 0 {
            return Ok(false);
        }
        let end = address
            .checked_add(length)
            .ok_or(UserAccessError::Overflow)?;
        if end > self.trampoline() || address < self.base.max(PAGE_SIZE) {
            return Err(UserAccessError::Fault);
        }
        let mut cursor = address;
        let mut executable = false;
        while cursor < end {
            let area = self.area_at(cursor).ok_or(UserAccessError::Fault)?;
            if !area.permission.contains(permission) {
                return Err(UserAccessError::Fault);
            }
            executable |= area.permission.contains(MapPermission::X);
            cursor = area.end;
        }
        Ok(executable)
    }

    fn is_current(&self) -> bool {
        crate::task::current_task().is_some_and(|task| task.maps_process(&self.process))
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
    pub(crate) fn copy_from_user(
        &self,
        address: usize,
        output: &mut [u8],
    ) -> Result<(), UserAccessError> {
        self.check_access(address, output.len(), MapPermission::R)?;
        if self.is_current() {
            // SAFETY: 已校验应用地址；output 是独占服务缓冲。
            return unsafe { read_local(address, output.as_mut_ptr(), output.len()) };
        }
        self.read_raw(address, output)
    }

    /// 读取并初始化服务缓冲；只有成功后才允许读取完整结果。
    pub(crate) fn copy_from_user_uninit(
        &self,
        address: usize,
        output: &mut [MaybeUninit<u8>],
    ) -> Result<(), UserAccessError> {
        self.check_access(address, output.len(), MapPermission::R)?;
        if self.is_current() {
            // SAFETY: 当前承载线程映射目标成员，调用方持有 MM 锁，VMA 已校验。
            // output 位于独占服务存储，应用指针没有 Rust 引用。
            return unsafe { read_local(address, output.as_mut_ptr().cast(), output.len()) };
        }
        for byte in output.iter_mut() {
            byte.write(0);
        }
        // SAFETY: 远程访问接口接收已初始化切片；上面已初始化全部字节。
        let initialized =
            unsafe { core::slice::from_raw_parts_mut(output.as_mut_ptr().cast(), output.len()) };
        self.read_raw(address, initialized)
    }

    /// 从用户地址读取 `length` 字节到新缓冲。
    ///
    /// # Errors
    ///
    /// 区间不可读、溢出或内存不足。
    pub(crate) fn read_user_bytes(
        &self,
        address: usize,
        length: usize,
    ) -> Result<Vec<u8>, UserAccessError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| UserAccessError::OutOfMemory)?;
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
        let executable = self.check_access(address, input.len(), MapPermission::W)?;
        self.write_checked(address, input, !executable && self.is_current())
    }

    fn write_checked(
        &self,
        address: usize,
        input: &[u8],
        local: bool,
    ) -> Result<(), UserAccessError> {
        if local {
            // SAFETY: MM 锁下已经校验整个应用区间，且当前承载线程映射的是同一个成员。
            // input 是独立的服务存储；可执行映射走原有内核写入和指令翻译撤销。
            return copy_result(unsafe {
                crate::arch::user_copy::copy(address as *mut u8, input.as_ptr(), input.len(), true)
            });
        }
        self.write_raw(address, input)
    }

    /// 把用户区间清零。
    ///
    /// # Errors
    ///
    /// 区间不可写或溢出。
    pub(crate) fn zero_user(&self, address: usize, length: usize) -> Result<(), UserAccessError> {
        let executable = self.check_access(address, length, MapPermission::W)?;
        let local = !executable && self.is_current();
        let zeros = [0u8; 512];
        let mut done = 0;
        while done < length {
            let chunk = (length - done).min(zeros.len());
            self.write_checked(address + done, &zeros[..chunk], local)?;
            done += chunk;
        }
        Ok(())
    }

    /// 校验用户区间可写（不访问内容）。
    ///
    /// # Errors
    ///
    /// 区间不可写或溢出。
    pub(crate) fn validate_user_write(
        &self,
        address: usize,
        length: usize,
    ) -> Result<(), UserAccessError> {
        self.check_access(address, length, MapPermission::W)
            .map(|_| ())
    }

    /// 读取以 NUL 结尾的用户字符串（不含 NUL），最多 `limit` 字节。
    ///
    /// # Errors
    ///
    /// 不可读（`Fault`）、超过上限仍未结束（`Unterminated`）、内存不足。
    pub(crate) fn copy_user_c_string(
        &self,
        address: usize,
        limit: usize,
    ) -> Result<Vec<u8>, UserAccessError> {
        let mut string = Vec::new();
        let mut cursor = address;
        let mut chunk = [0u8; 256];
        loop {
            // 每段不跨页：页之后可能未映射，而字符串在本页内已经结束。
            let page_remaining = PAGE_SIZE - (cursor % PAGE_SIZE);
            let length = chunk
                .len()
                .min(page_remaining)
                .min(limit.saturating_sub(string.len()).max(1));
            self.copy_from_user(cursor, &mut chunk[..length])?;
            if let Some(end) = chunk[..length].iter().position(|&byte| byte == 0) {
                string
                    .try_reserve(end)
                    .map_err(|_| UserAccessError::OutOfMemory)?;
                string.extend_from_slice(&chunk[..end]);
                return Ok(string);
            }
            if string.len() + length > limit {
                return Err(UserAccessError::Unterminated);
            }
            string
                .try_reserve(length)
                .map_err(|_| UserAccessError::OutOfMemory)?;
            string.extend_from_slice(&chunk[..length]);
            cursor = cursor
                .checked_add(length)
                .ok_or(UserAccessError::Overflow)?;
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
        // futex 的对齐字读取不能由逐字节的通用复制例程实现。
        self.check_access(address, bytes.len(), MapPermission::R)?;
        self.read_raw(address, &mut bytes)?;
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
    pub(crate) fn user_atomic_u32(
        &self,
        address: usize,
        write: bool,
    ) -> Result<UserAtomicU32<'_>, UserAccessError> {
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
    pub(crate) fn compare_exchange_user_u32(
        &self,
        address: usize,
        current: u32,
        replacement: u32,
    ) -> Result<Result<u32, u32>, UserAccessError> {
        Ok(self
            .user_atomic_u32(address, true)?
            .compare_exchange(current, replacement))
    }

    /// 为 futex 地址生成等待身份。
    ///
    /// # Errors
    ///
    /// 未对齐、越界或（共享 key 时）不可读。
    pub(crate) fn futex_key(
        &self,
        address: usize,
        address_space: usize,
        private: bool,
    ) -> Result<FutexKey, UserAccessError> {
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
        Ok(FutexKey::Private {
            address_space,
            address,
        })
    }

    /// 调试访问（ptrace、`/proc/<pid>/mem`）：忽略 Linux 权限，只要求已映射；写只读私有映射时内核为本进程
    /// 复制该页（与 Linux `FOLL_FORCE` 相同）。返回实际传输字节数（遇到未映射处截断）。
    pub(crate) fn access_forced(&self, address: usize, buffer: &mut [u8], write: bool) -> usize {
        let mut done = 0;
        while done < buffer.len() {
            let Some(area) = self.area_at(address + done) else {
                break;
            };
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

/// SAFETY: 调用方保证应用区间在当前承载线程中，持 MM 锁并已校验权限；
/// output 指向与应用区间不重叠的独占服务缓冲。
unsafe fn read_local(
    address: usize,
    output: *mut u8,
    length: usize,
) -> Result<(), UserAccessError> {
    // SAFETY: 调用方保证当前地址空间、区间权限及缓冲的可写性。
    copy_result(unsafe {
        crate::arch::user_copy::copy(output, address as *const u8, length, false)
    })
}

fn copy_result(complete: bool) -> Result<(), UserAccessError> {
    if complete {
        Ok(())
    } else {
        Err(UserAccessError::Fault)
    }
}
