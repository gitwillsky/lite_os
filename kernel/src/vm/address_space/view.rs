//! 共享进程成员的两套执行视图及其地址边界。

use super::{
    AddressSpace, AddressSpaceToken, Arc, ExecutionView, FaultAccess, FaultSource, VmError,
};

impl AddressSpace {
    /// 返回用户态时激活的页表 token。
    pub(crate) fn token(&self, view: ExecutionView) -> AddressSpaceToken {
        match view {
            ExecutionView::Normal => self.token.clone(),
            ExecutionView::Private => self
                .private_token
                .as_ref()
                .expect("restricted mode requires a shared-process member")
                .clone(),
        }
    }

    /// 共享进程成员具有独立 private 硬件根；普通/共享模板进程不能进入受限模式。
    pub(crate) fn has_private_view(&self) -> bool {
        self.private_token.is_some()
    }

    /// 地址是否属于共享进程成员的私有区。
    pub(crate) fn is_private_address(&self, address: usize) -> bool {
        self.has_private_view() && self.range.contains(&address)
    }

    /// 地址是否属于成员可见的共享区。
    pub(crate) fn is_shared_address(&self, address: usize) -> bool {
        self.shared
            .as_ref()
            .is_some_and(|space| space.range.contains(&address))
    }

    /// 受限缺页不能解析共享区；否则隐藏区会在下一次重试前被错误补入或无限重试。
    ///
    /// # Errors
    ///
    /// 地址超出私有区，或权限、映射和 backing 无法解析这次缺页。
    pub(crate) fn fault_private(
        self: &Arc<Self>,
        address: usize,
        access: FaultAccess,
    ) -> Result<FaultSource, VmError> {
        if !self.is_private_address(address) {
            return Err(VmError::AccessDenied);
        }
        self.fault(address, access)
    }
}
