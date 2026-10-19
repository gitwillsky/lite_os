//! 内存型inode的实际xattr内容；锁与ctime归inode现有state，不另建属性锁或影子权限表。
use super::permission::OwnerModeState;
use super::{AccessIdentity, FileSystemError, Inode, InodeMetadata, InodeType};
use crate::fallible_tree::FallibleMap;
use crate::namespace::UserNamespace;
use alloc::{sync::Arc, vec::Vec};
use syscall_abi::{
    capability::Capability,
    file_capability::{self, FileCapabilityData},
};

/// xattr域的存储/权限错误；rootid不能投影到caller时保留独立EOVERFLOW，不改普通FS errno。
#[derive(Debug)]
pub(crate) enum XattrError {
    Filesystem(FileSystemError),
    Overflow,
}
impl From<FileSystemError> for XattrError {
    fn from(error: FileSystemError) -> Self {
        Self::Filesystem(error)
    }
}

/// 一次xattr mutation的不可变caller/SB authority与owned value；backend在实际inode gate内授权。
/// 不是另一份inode状态：UID/GID/mode只从持锁backend传入，禁止VFS快照授权后裸写。
pub(crate) struct XattrChange<'a> {
    pub(super) name: &'a [u8],
    pub(super) value: Option<Vec<u8>>,
    pub(super) flags: u32,
    pub(super) identity: &'a AccessIdentity,
    pub(super) filesystem_user: Arc<UserNamespace>,
}
impl XattrChange<'_> {
    /// 实际path producer提供SB creator与当前caller；value已完成user-copy，inode facts由backend补齐。
    pub(crate) fn new<'a>(
        name: &'a [u8],
        value: Option<Vec<u8>>,
        flags: u32,
        identity: &'a AccessIdentity,
        filesystem_user: Arc<UserNamespace>,
    ) -> XattrChange<'a> {
        XattrChange {
            name,
            value,
            flags,
            identity,
            filesystem_user,
        }
    }
    /// 转换非空capability→inode可写/权限→映射→handler/security授权，任何失败不发表属性。
    pub(super) fn prepare(
        &mut self,
        owner: &OwnerModeState,
        writable: Result<(), FileSystemError>,
    ) -> Result<(), FileSystemError> {
        if self.name == file_capability::NAME
            && self.value.as_ref().is_some_and(|value| !value.is_empty())
        {
            self.value = Some(prepare_capability(
                self.value.take().expect("nonempty capability"),
                owner,
                self.identity,
                &self.filesystem_user,
            )?);
        }
        writable?;
        require_permission(owner, self.identity, self.name, true)?;
        if self
            .filesystem_user
            .map_kernel_id(true, owner.uid())
            .is_none()
            || self
                .filesystem_user
                .map_kernel_id(false, owner.gid())
                .is_none()
        {
            return Err(FileSystemError::PermissionDenied);
        }
        require_handler(self.name)?;
        if self.name.starts_with(b"security.") {
            let permitted = if self.name == file_capability::NAME {
                // 非空set的SETFCAP由转换检查；空set绕过commoncap转换，remove仍需SETFCAP。
                self.value.is_some()
                    || self
                        .identity
                        .inode_capability(owner.uid(), owner.gid(), Capability::SETFCAP)
            } else {
                self.identity
                    .namespace_capable(&self.filesystem_user, Capability::SYS_ADMIN)
            };
            if !permitted {
                return Err(FileSystemError::PermissionDenied);
            }
        }
        Ok(())
    }
}

/// fixed commoncap的root匹配：实际caller namespace或任意ancestor将此global kuid映射成UID0。
pub(super) fn root_in_namespace(root: u32, user: &Arc<UserNamespace>) -> bool {
    let mut current = Some(user.clone());
    while let Some(namespace) = current {
        if namespace.map_kernel_id(true, root) == Some(0) {
            return true;
        }
        current = namespace.parent();
    }
    false
}

/// 固定VFS xattr_permission；security/system由filesystem/security policy处理，user/trusted按实际caller。
pub(super) fn require_access(
    inode: &dyn Inode,
    identity: &AccessIdentity,
    name: &[u8],
    write: bool,
) -> Result<InodeMetadata, FileSystemError> {
    let metadata = inode.metadata()?;
    if write {
        inode.check_xattr_write()?;
    }
    require_permission(
        &OwnerModeState::new(
            metadata.kind,
            metadata.mode as u16,
            metadata.uid,
            metadata.gid,
        ),
        identity,
        name,
        write,
    )?;
    Ok(metadata)
}

fn require_permission(
    owner: &OwnerModeState,
    identity: &AccessIdentity,
    name: &[u8],
    write: bool,
) -> Result<(), FileSystemError> {
    if name.is_empty() || name.contains(&0) {
        return Err(FileSystemError::InvalidOperation);
    }
    if name.len() > 255 {
        return Err(FileSystemError::NameTooLong);
    }
    if name.starts_with(b"security.") || name.starts_with(b"system.") {
        return Ok(());
    }
    let denied = || {
        if write {
            FileSystemError::PermissionDenied
        } else {
            FileSystemError::NotFound
        }
    };
    if name.starts_with(b"trusted.") {
        if !identity.has_capability(Capability::SYS_ADMIN) {
            return Err(denied());
        }
        return Ok(());
    }
    if name.starts_with(b"user.") {
        match owner.kind() {
            InodeType::File | InodeType::Socket => {}
            InodeType::Directory => {
                if write && owner.mode() & 0o1000 != 0 && !identity.owns_inode(owner.uid()) {
                    return Err(FileSystemError::PermissionDenied);
                }
            }
            _ => return Err(denied()),
        }
    }
    identity.require_owner(owner, if write { 2 } else { 4 })
}

/// 当前支持的filesystem xattr handler namespace；system ACL需要实际ACL owner，不能作为普通bytes放行。
pub(super) fn require_handler(name: &[u8]) -> Result<(), FileSystemError> {
    for prefix in [b"user.".as_slice(), b"trusted.", b"security."] {
        if name.starts_with(prefix) {
            return if name.len() == prefix.len() {
                Err(FileSystemError::InvalidOperation)
            } else {
                Ok(())
            };
        }
    }
    Err(FileSystemError::NotSupported)
}

/// fixed cap_convert_nscap：validated非空setcap先检查inode-scoped SETFCAP，再映射实际FS creator namespace。
pub(super) fn prepare_capability(
    value: Vec<u8>,
    owner: &OwnerModeState,
    identity: &AccessIdentity,
    filesystem_user: &Arc<UserNamespace>,
) -> Result<Vec<u8>, FileSystemError> {
    let mut caps =
        FileCapabilityData::for_setxattr(&value).map_err(|_| FileSystemError::InvalidOperation)?;
    if !identity.inode_capability(owner.uid(), owner.gid(), Capability::SETFCAP) {
        return Err(FileSystemError::PermissionDenied);
    }
    if caps.magic & file_capability::REVISION_MASK == file_capability::REVISION_2
        && identity.namespace_capable(filesystem_user, Capability::SETFCAP)
    {
        return Ok(value);
    }
    let root = identity
        .user_namespace()
        .to_kernel(true, caps.root_uid)
        .ok_or(FileSystemError::InvalidOperation)?;
    caps.root_uid = filesystem_user
        .map_kernel_id(true, root)
        .ok_or(FileSystemError::InvalidOperation)?;
    caps.magic = file_capability::REVISION_3 | (caps.magic & file_capability::EFFECTIVE);
    let (bytes, length) = caps
        .encode()
        .map_err(|_| FileSystemError::InvalidOperation)?;
    copy(&bytes[..length])
}

/// cap_inode_getsecurity：caller-visible root非零返回revision3，当前/ancestor root返回revision2。
pub(super) fn project_capability(
    value: Vec<u8>,
    filesystem_user: &Arc<UserNamespace>,
    caller: &Arc<UserNamespace>,
) -> Result<Vec<u8>, XattrError> {
    let mut caps =
        FileCapabilityData::for_setxattr(&value).map_err(|_| FileSystemError::InvalidOperation)?;
    let root = filesystem_user
        .to_kernel(true, caps.root_uid)
        .ok_or(XattrError::Overflow)?;
    if let Some(local) = caller.map_kernel_id(true, root).filter(|local| *local != 0) {
        caps.magic = file_capability::REVISION_3 | (caps.magic & file_capability::EFFECTIVE);
        caps.root_uid = local;
    } else {
        if !root_in_namespace(root, caller) {
            return Err(XattrError::Overflow);
        }
        caps.magic = file_capability::REVISION_2 | (caps.magic & file_capability::EFFECTIVE);
        caps.root_uid = 0;
    }
    let (bytes, length) = caps
        .encode()
        .map_err(|_| FileSystemError::InvalidOperation)?;
    Ok(copy(&bytes[..length])?)
}

pub(super) const CREATE: u32 = 1;
pub(super) const REPLACE: u32 = 2;
pub(super) const MAX_VALUE_SIZE: usize = 65_536;

/// tmpfs inode state持有的唯一attribute表；key/value只有owned bytes，无namespace/task引用。
pub(super) struct Xattrs {
    entries: FallibleMap<Vec<u8>, Vec<u8>>,
}
impl Xattrs {
    pub(super) const fn new() -> Self {
        Self {
            entries: FallibleMap::new(),
        }
    }
    /// 按完整attribute名称读取owned快照；missing与分配失败均不改变表。
    pub(super) fn get(&self, name: &[u8]) -> Result<Vec<u8>, FileSystemError> {
        let value = self.entries.get(name).ok_or(FileSystemError::NotFound)?;
        copy(value)
    }
    /// Linux listxattr的NUL分隔名称；调用者提供实际权限过滤，不复制attribute集合。
    pub(super) fn list(&self, visible: impl Fn(&[u8]) -> bool) -> Result<Vec<u8>, FileSystemError> {
        let mut names = Vec::new();
        for (name, _) in self.entries.iter() {
            if !visible(name) {
                continue;
            }
            names
                .try_reserve(name.len() + 1)
                .map_err(|_| FileSystemError::OutOfMemory)?;
            names.extend_from_slice(name);
            names.push(0);
        }
        Ok(names)
    }
    /// Caller持inode state；已copy的value直接移交，不重复复制64KiB user buffer。
    /// Some(empty)是空属性，None是删除；flags3按已存在/缺失分别返回EEXIST/ENODATA，不能提前EINVAL。
    /// 返回替换/删除的旧byte buffer；caller可在释放inode锁后析构。
    pub(super) fn change(
        &mut self,
        name: &[u8],
        value: Option<Vec<u8>>,
        flags: u32,
    ) -> Result<Option<Vec<u8>>, FileSystemError> {
        if flags & !(CREATE | REPLACE) != 0 || name.is_empty() || name.contains(&0) {
            return Err(FileSystemError::InvalidOperation);
        }
        if name.len() > 255 {
            return Err(FileSystemError::NameTooLong);
        }
        if value
            .as_ref()
            .is_some_and(|value| value.len() > MAX_VALUE_SIZE)
        {
            return Err(FileSystemError::InvalidOperation);
        }
        if let Some(old) = self.entries.get_mut(name) {
            if flags & CREATE != 0 {
                return Err(FileSystemError::AlreadyExists);
            }
            return Ok(match value {
                Some(value) => Some(core::mem::replace(old, value)),
                None => self.entries.remove(name),
            });
        }
        if flags & REPLACE != 0 {
            return Err(FileSystemError::NotFound);
        }
        let Some(value) = value else {
            return Ok(None);
        };
        self.entries
            .try_insert(copy(name)?, value)
            .map_err(|_| FileSystemError::OutOfMemory)
    }
    /// killpriv只摘实际capability属性；不分配、不调用外部权限或另一份xattr状态。
    pub(super) fn kill_capability(&mut self) -> Option<Vec<u8>> {
        let name = syscall_abi::file_capability::NAME;
        // cap_inode_need_killpriv只在值长度>0时删除；空attribute仍是标准可读的空值。
        if self
            .entries
            .get(name)
            .is_some_and(|value| !value.is_empty())
        {
            self.entries.remove(name)
        } else {
            None
        }
    }
}

fn copy(bytes: &[u8]) -> Result<Vec<u8>, FileSystemError> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(bytes.len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    output.extend_from_slice(bytes);
    Ok(output)
}
