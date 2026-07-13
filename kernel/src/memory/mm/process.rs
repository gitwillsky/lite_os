use alloc::vec::Vec;

use super::{MemorySet, UserAccessError};

impl MemorySet {
    /// @description 从当前 Linux mm argument range 复制 NUL 分隔的 argv bytes。
    /// @return 成功返回可变用户栈中的当前 argv；range 不可读时返回明确 user-access error。
    /// @errors range overflow、用户映射不可读或 kernel buffer allocation 失败。
    pub(crate) fn process_arguments(&mut self) -> Result<Vec<u8>, UserAccessError> {
        let length = self
            .argument_range
            .end
            .checked_sub(self.argument_range.start)
            .ok_or(UserAccessError::Overflow)?;
        let mut arguments = Vec::new();
        arguments
            .try_reserve_exact(length)
            .map_err(|_| UserAccessError::OutOfMemory)?;
        arguments.resize(length, 0);
        self.copy_from_user(self.argument_range.start, &mut arguments)?;
        Ok(arguments)
    }
}
