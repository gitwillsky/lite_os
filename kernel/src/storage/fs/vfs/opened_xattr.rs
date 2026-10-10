//! 实际path上的xattr权限与file-cap namespace投影；raw attributes仍由实际inode独占。
use super::super::{XattrChange, XattrError, xattr};
use super::{AccessIdentity, FileSystemError, OpenedFile};
use crate::namespace::UserNamespace;
use alloc::{sync::Arc, vec::Vec};
use syscall_abi::file_capability;

impl OpenedFile {
    fn xattr_namespace(&self) -> Arc<UserNamespace> {
        self.mount
            .as_ref()
            .map_or_else(crate::namespace::initial_user, |view| {
                view.filesystem.user.clone()
            })
    }
    /// getxattr使用当前caller权限和namespace rootid投影；不采用opener凭据或按inode猜mount。
    pub(crate) fn read_xattr(
        &self,
        name: &[u8],
        identity: &AccessIdentity,
    ) -> Result<Vec<u8>, XattrError> {
        let inode = self.inode();
        xattr::require_access(&*inode, identity, name, false)?;
        xattr::require_handler(name)?;
        let value = inode.read_xattr(name)?;
        if name == file_capability::NAME {
            xattr::project_capability(value, &self.xattr_namespace(), &identity.user_namespace())
        } else {
            Ok(value)
        }
    }
    /// listxattr不额外授予read内容权限；filesystem过滤实际caller不可见的trusted名称。
    pub(crate) fn list_xattrs(&self, identity: &AccessIdentity) -> Result<Vec<u8>, XattrError> {
        Ok(self.inode().list_xattrs(identity)?)
    }
    /// set/remove共用实际mount writer与inode mutation；准备失败不发表attribute或通知。
    /// capability非空set按fixed commoncap转换；remove需要inode-scoped SETFCAP，空set遵循Linux特殊路径。
    pub(crate) fn change_xattr(
        self: &Arc<Self>,
        name: &[u8],
        value: Option<Vec<u8>>,
        flags: u32,
        identity: &AccessIdentity,
    ) -> Result<(), XattrError> {
        let _writer = self.begin_write()?;
        let inode = self.inode();
        inode.change_xattr(XattrChange::new(
            name,
            value,
            flags,
            identity,
            self.xattr_namespace(),
        ))?;
        super::super::notify_opened(self, super::super::IN_ATTRIB);
        Ok(())
    }
    /// exec只消费raw disk capability与实际SB ancestry；getxattr的caller-visible revision转换不参与exec。
    pub(crate) fn executable_capability(
        &self,
        namespace: &super::MountNamespace,
        caller: &Arc<UserNamespace>,
    ) -> Result<Option<file_capability::FileCapabilityData>, FileSystemError> {
        if !self.may_suid(namespace, caller) {
            return Ok(None);
        }
        let value = match self.inode().read_xattr(file_capability::NAME) {
            Ok(value) => value,
            Err(FileSystemError::NotFound | FileSystemError::NotSupported) => return Ok(None),
            Err(error) => return Err(error),
        };
        let caps = file_capability::FileCapabilityData::from_disk(&value)
            .map_err(|_| FileSystemError::InvalidOperation)?;
        let Some(root) = self.xattr_namespace().to_kernel(true, caps.root_uid) else {
            return Ok(None);
        };
        Ok(xattr::root_in_namespace(root, caller).then_some(caps))
    }
}
