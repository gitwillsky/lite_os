//! 默认全局0644 sysctl的Linux test_perm；按GLOBAL_ROOT_EUID选owner，无inode DAC capability豁免。
use super::super::FileSystemError;
pub(super) fn require(effective_uid: u32, requested: u8) -> Result<(), FileSystemError> {
    let granted = if effective_uid == 0 { 6 } else { 4 };
    if requested & granted != requested {
        Err(FileSystemError::AccessDenied)
    } else {
        Ok(())
    }
}

use super::ProcNode;

/// 校验 sysctl 目录和只读 boot_id 的权限，其余 inode 交由各自的权限规则处理。
///
/// 返回 None 表示该 inode 不适用。请求超出允许的访问位时返回 AccessDenied。
pub(super) fn require_directory_or_boot_id(
    node: ProcNode,
    requested: u8,
) -> Option<Result<(), FileSystemError>> {
    let granted = match node {
        ProcNode::SysDir
        | ProcNode::NetSysctlDir
        | ProcNode::CoreNetSysctlDir
        | ProcNode::UserSysctlDir
        | ProcNode::FsSysctlDir
        | ProcNode::InotifySysctlDir
        | ProcNode::KernelSysctlDir
        | ProcNode::RandomSysctlDir => 5,
        ProcNode::BootId => 4,
        _ => return None,
    };
    Some(if requested & granted == requested {
        Ok(())
    } else {
        Err(FileSystemError::AccessDenied)
    })
}
