//! 缺页访问类型。

use super::page_table::PagePermissions;

/// 触发缺页的访问。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FaultAccess {
    Read,
    Write,
    Execute,
}

impl FaultAccess {
    /// 该访问要求映射具备的权限。
    pub(super) fn required(self) -> PagePermissions {
        match self {
            Self::Read => PagePermissions::READ,
            Self::Write => PagePermissions::WRITE,
            Self::Execute => PagePermissions::EXECUTE,
        }
    }
}
