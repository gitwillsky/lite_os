//! 标准 fusectl：连接目录是挂载的弱索引，参数与请求状态仍由 Connection 拥有。
use crate::fs::chardev::DeviceFile;
use alloc::sync::{Arc, Weak};
use spin::Mutex;
use syscall_abi::{capability::Capability, errno};

use super::{
    connection::{Connection, Limit, unprivileged_limit},
    filesystem::Mount,
};
use crate::{
    fallible_tree::FallibleMap,
    fs::{
        self, AccessIdentity, CreateMetadata, DataBacking, DirectoryEntry, DirectoryRead,
        DirectoryVisitor, FileSystem, FileSystemError, FileSystemStatistics, IndexedDirectory,
        Inode, InodeMetadata, InodeOpen, InodeOpenError, InodeType, SuperblockFacts, chardev,
        mount_options,
    },
};

struct Registry {
    mounts: FallibleMap<usize, Weak<Mount>>,
    filesystem: Option<Arc<ControlFileSystem>>,
}
// OWNER: 唯一 fusectl 视图与存活挂载索引；Weak 不延长 FUSE 生命周期，短锁不跨 user-copy/连接操作。
// 缺失该索引会无法列举连接，保存强引用则会让卸载后的守护进程/挂载无法释放。
static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    mounts: FallibleMap::new(),
    filesystem: None,
});

/// 登记已完整建立的 `mount`；返回 ()，索引节点分配失败返回 OutOfMemory。
pub(super) fn register(mount: &Arc<Mount>) -> Result<(), FileSystemError> {
    let slot = FallibleMap::try_reserve_node().map_err(|_| FileSystemError::OutOfMemory)?;
    // ID 由 filesystem allocator 唯一分配；在短锁内只提交预留节点，不让 direct reclaim 重入索引。
    REGISTRY
        .lock()
        .mounts
        .commit_vacant(slot.fill(mount.filesystem_id, Arc::downgrade(mount)));
    Ok(())
}
/// 撤销 `id` 的公开目录；幂等，无分配，不中止连接（由实际 filesystem shutdown 拥有）。
pub(super) fn unregister(id: usize) {
    REGISTRY.lock().mounts.remove(&id);
}

fn mount(id: usize) -> Option<Arc<Mount>> {
    let mount = REGISTRY.lock().mounts.get(&id).cloned();
    mount.and_then(|mount| mount.upgrade())
}
fn connection(id: usize) -> Option<Arc<Connection>> {
    mount(id).map(|mount| mount.connection.clone())
}

#[derive(Clone, Copy)]
enum Field {
    Waiting,
    Abort,
    Maximum,
    Congestion,
}
impl Field {
    fn name(self) -> &'static [u8] {
        match self {
            Self::Waiting => b"waiting",
            Self::Abort => b"abort",
            Self::Maximum => b"max_background",
            Self::Congestion => b"congestion_threshold",
        }
    }
    fn limit(self) -> Option<Limit> {
        match self {
            Self::Maximum => Some(Limit::Maximum),
            Self::Congestion => Some(Limit::Congestion),
            _ => None,
        }
    }
    fn mode(self) -> u32 {
        match self {
            Self::Waiting => 0o400,
            Self::Abort => 0o200,
            _ => 0o600,
        }
    }
    fn number(self) -> u64 {
        match self {
            Self::Waiting => 1,
            Self::Abort => 2,
            Self::Maximum => 3,
            Self::Congestion => 4,
        }
    }
}
const FIELDS: [Field; 4] = [
    Field::Waiting,
    Field::Abort,
    Field::Maximum,
    Field::Congestion,
];

#[derive(Clone, Copy)]
enum Node {
    Root,
    Connection,
    File(Field),
}

struct ControlInode {
    filesystem: usize,
    id: usize,
    uid: u32,
    gid: u32,
    node: Node,
}
impl ControlInode {
    fn new(
        filesystem: usize,
        id: usize,
        uid: u32,
        gid: u32,
        node: Node,
    ) -> Result<Arc<Self>, FileSystemError> {
        Arc::try_new(Self {
            filesystem,
            id,
            uid,
            gid,
            node,
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }
    fn number(&self) -> u64 {
        match self.node {
            Node::Root => 1,
            Node::Connection => self.id as u64 * 8,
            Node::File(field) => self.id as u64 * 8 + field.number(),
        }
    }
    fn child(&self, node: Node) -> Result<Arc<Self>, FileSystemError> {
        Self::new(self.filesystem, self.id, self.uid, self.gid, node)
    }
}
impl Inode for ControlInode {
    fn filesystem_id(&self) -> usize {
        self.filesystem
    }
    fn inode_type(&self) -> InodeType {
        if matches!(self.node, Node::File(_)) {
            InodeType::File
        } else {
            InodeType::Directory
        }
    }
    fn metadata(&self) -> Result<InodeMetadata, FileSystemError> {
        Ok(InodeMetadata {
            filesystem: self.filesystem as u64,
            inode: self.number(),
            kind: self.inode_type(),
            mode: match self.node {
                Node::Root => 0o040755,
                Node::Connection => 0o040500,
                Node::File(field) => 0o100000 | field.mode(),
            },
            links: if matches!(self.node, Node::File(_)) {
                1
            } else {
                2
            },
            uid: self.uid,
            gid: self.gid,
            size: 0,
            blocks: 0,
            block_size: 4096,
            atime: 0,
            mtime: 0,
            ctime: 0,
            fine_times: Default::default(),
            device: None,
        })
    }
    fn size(&self) -> u64 {
        0
    }
    fn is_executable(&self) -> bool {
        false
    }
    fn data_backing(&self) -> DataBacking {
        DataBacking::Snapshot
    }
    fn read_storage(&self, _: u64, _: &mut [u8]) -> Result<usize, FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }
    fn write_storage(
        &self,
        _: u64,
        _: &[u8],
        _: Option<&AccessIdentity>,
    ) -> Result<usize, FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }
    fn append_storage(
        &self,
        _: &[u8],
        _: Option<&AccessIdentity>,
    ) -> Result<(u64, usize), FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }
    fn truncate_storage(
        &self,
        size: u64,
        _: Option<&AccessIdentity>,
    ) -> Result<(), FileSystemError> {
        // shell 重定向带 O_TRUNC；控制文件 i_size 恒为零，清零不改变后台参数或触发 abort。
        if size == 0 {
            Ok(())
        } else {
            Err(FileSystemError::InvalidOperation)
        }
    }
    fn create(
        &self,
        _: &[u8],
        _: InodeType,
        _: CreateMetadata,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }
    fn unlink(&self, _: &[u8], _: bool) -> Result<(), FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }
    fn rename(&self, _: &[u8], _: u64, _: &[u8], _: bool) -> Result<(), FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }
    fn sync_storage(&self) -> Result<(), FileSystemError> {
        Ok(())
    }
    fn open_backend(
        &self,
        _flags: u32,
        _: &AccessIdentity,
        _: &crate::namespace::NamespaceAuthority,
    ) -> Result<InodeOpen, InodeOpenError> {
        let Node::File(field) = self.node else {
            return Ok(InodeOpen::Storage(None));
        };

        let file = Arc::try_new(ControlFile {
            id: self.id,
            field,
            waiting: Mutex::new(None),
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        Ok(InodeOpen::Device(file))
    }
    fn find_child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
        if name == b"." {
            return Ok(self.child(self.node)?);
        }
        if name == b".." {
            return Ok(Self::new(self.filesystem, 0, 0, 0, Node::Root)?);
        }
        match self.node {
            Node::Root => {
                if name.is_empty() || name[0] == b'0' || !name.iter().all(u8::is_ascii_digit) {
                    return Err(FileSystemError::NotFound);
                }
                let id = core::str::from_utf8(name)
                    .ok()
                    .and_then(|name| name.parse::<usize>().ok())
                    .ok_or(FileSystemError::NotFound)?;
                let mount = mount(id).ok_or(FileSystemError::NotFound)?;
                Ok(Self::new(
                    self.filesystem,
                    id,
                    mount.options.user_id,
                    mount.options.group_id,
                    Node::Connection,
                )?)
            }
            Node::Connection => {
                if mount(self.id).is_none() {
                    return Err(FileSystemError::NotFound);
                }
                let field = FIELDS
                    .into_iter()
                    .find(|field| field.name() == name)
                    .ok_or(FileSystemError::NotFound)?;
                Ok(self.child(Node::File(field))?)
            }
            Node::File(_) => Err(FileSystemError::NotDirectory),
        }
    }
    fn read_directory(
        &self,
        cursor: u64,
        visitor: &mut dyn DirectoryVisitor,
    ) -> Result<DirectoryRead, FileSystemError> {
        if matches!(self.node, Node::File(_)) {
            return Err(FileSystemError::NotDirectory);
        }
        let mut stream = IndexedDirectory::new(cursor, visitor);
        let emit = |stream: &mut IndexedDirectory<'_>, index, inode, name: &[u8], kind| {
            stream.emit(index, DirectoryEntry { inode, name, kind })
        };
        if !emit(&mut stream, 0, self.number(), b".", InodeType::Directory)?
            || !emit(&mut stream, 1, 1, b"..", InodeType::Directory)?
        {
            return Ok(stream.finish());
        }
        match self.node {
            Node::Root => {
                // 短锁内每次只复制一个 Weak；分配名称、升级/释放 Mount 与 visitor 均在锁外。
                let mut previous = None;
                let mut index = 2;
                loop {
                    let entry = {
                        let registry = REGISTRY.lock();
                        registry
                            .mounts
                            .iter_from(&previous.unwrap_or(0))
                            .find(|(id, _)| previous.is_none_or(|previous| **id > previous))
                            .map(|(id, mount)| (*id, mount.clone()))
                    };
                    let Some((id, mount)) = entry else {
                        break;
                    };
                    previous = Some(id);
                    let entry_index = index;
                    index += 1;
                    if mount.upgrade().is_none() {
                        continue;
                    }
                    let name = fs::try_format_bytes(format_args!("{id}"))?;
                    if !emit(
                        &mut stream,
                        entry_index,
                        id as u64 * 8,
                        &name,
                        InodeType::Directory,
                    )? {
                        break;
                    }
                }
            }
            Node::Connection => {
                if mount(self.id).is_some() {
                    for (index, field) in FIELDS.into_iter().enumerate() {
                        if !emit(
                            &mut stream,
                            index + 2,
                            self.id as u64 * 8 + field.number(),
                            field.name(),
                            InodeType::File,
                        )? {
                            break;
                        }
                    }
                }
            }
            Node::File(_) => unreachable!(),
        }
        Ok(stream.finish())
    }
}

struct ControlFile {
    id: usize,
    field: Field,
    // OWNER: Linux waiting 的一次 open/read 快照，防止分段读把变化中的数字拼成错误数值；不复制 live 请求事实。
    waiting: Mutex<Option<usize>>,
}
impl chardev::DeviceFile for ControlFile {
    fn positioned(&self) -> Option<&dyn chardev::PositionedFile> {
        Some(self)
    }
    fn supports_read(&self) -> bool {
        !matches!(self.field, Field::Abort)
    }
    fn supports_write(&self) -> bool {
        !matches!(self.field, Field::Waiting)
    }
    fn poll(&self, events: i16) -> i16 {
        events & 0x145
    }
}
impl chardev::PositionedFile for ControlFile {
    fn advances_write_position(&self) -> bool {
        false
    }
    fn supports_positioned_io(&self) -> bool {
        self.field.limit().is_some()
    }
    fn read_at(
        &self,
        output: &mut dyn chardev::UserOutput,
        offset: u64,
    ) -> Result<(), chardev::DeviceError> {
        if !self.supports_read() {
            return Err(chardev::DeviceError::Errno(errno::EINVAL));
        }
        if output.remaining() == 0 {
            return Ok(());
        }
        let value = if matches!(self.field, Field::Waiting) {
            if offset == 0 {
                let Some(connection) = connection(self.id) else {
                    return Ok(());
                };
                let value = connection.waiting();
                *self.waiting.lock() = Some(value);
                value
            } else {
                self.waiting.lock().unwrap_or(0)
            }
        } else {
            let Some(connection) = connection(self.id) else {
                return Ok(());
            };
            usize::from(connection.limit(self.field.limit().expect("readable limit")))
        };
        let bytes = fs::try_format_bytes(format_args!("{value}\n"))
            .map_err(|_| chardev::DeviceError::Errno(errno::ENOMEM))?;
        let offset = usize::try_from(offset).unwrap_or(usize::MAX);
        if offset < bytes.len() {
            let end = bytes.len().min(offset.saturating_add(output.remaining()));
            output
                .write(&bytes[offset..end])
                .map_err(|_| chardev::DeviceError::Errno(errno::EFAULT))?;
        }
        Ok(())
    }
    fn write_at(
        &self,
        input: &mut dyn chardev::UserInput,
        offset: u64,
        writer: &crate::namespace::NamespaceAuthority,
    ) -> Result<(), chardev::DeviceError> {
        if !self.supports_write() {
            return Err(chardev::DeviceError::Errno(errno::EINVAL));
        }
        let count = input.remaining();
        if let Some(limit) = self.field.limit() {
            if offset != 0 {
                return Err(chardev::DeviceError::Errno(errno::EINVAL));
            }
            let mut bytes = [0u8; 65];
            let length = count.min(bytes.len());
            input
                .copy(&mut bytes[..length])
                .map_err(|_| chardev::DeviceError::Errno(errno::EFAULT))?;
            let value = number(&bytes[..length])?;
            let maximum =
                if writer.capable(&crate::namespace::initial_user(), Capability::SYS_ADMIN) {
                    u16::MAX
                } else {
                    unprivileged_limit()
                };
            if value > u64::from(maximum) {
                return Err(chardev::DeviceError::Errno(errno::EINVAL));
            }
            if let Some(connection) = connection(self.id) {
                connection.set_limit(limit, value as u16);
            }
        } else if let Some(connection) = connection(self.id) {
            // abort 不解引用用户 buffer；卸载后旧 fd 的写仍返回原始 count，不能恢复连接。
            connection.abort(true);
        }
        input.consume(count);
        Ok(())
    }
    fn seek(&self, current: u64, offset: i64, whence: u32) -> Result<u64, chardev::DeviceError> {
        if self.field.limit().is_none() {
            return Err(chardev::DeviceError::Errno(errno::ESPIPE));
        }
        let base = match whence {
            0 | 2 => 0,
            1 => current,
            _ => return Err(chardev::DeviceError::Errno(errno::EINVAL)),
        };
        let next = i128::from(base) + i128::from(offset);
        if !(0..=i128::from(i64::MAX)).contains(&next) {
            return Err(chardev::DeviceError::Errno(errno::EINVAL));
        }
        Ok(next as u64)
    }
}

fn number(bytes: &[u8]) -> Result<u64, chardev::DeviceError> {
    let bytes = bytes.split(|byte| *byte == 0).next().unwrap_or_default();
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let text =
        core::str::from_utf8(bytes).map_err(|_| chardev::DeviceError::Errno(errno::EINVAL))?;
    let text = text.strip_prefix('+').unwrap_or(text);
    let (text, radix) = if text.starts_with("0x") || text.starts_with("0X") {
        (&text[2..], 16)
    } else if text.starts_with('0') {
        (text, 8)
    } else {
        (text, 10)
    };
    if text.starts_with('+') {
        return Err(chardev::DeviceError::Errno(errno::EINVAL));
    }
    u64::from_str_radix(text, radix).map_err(|error| {
        chardev::DeviceError::Errno(
            if matches!(error.kind(), core::num::IntErrorKind::PosOverflow) {
                errno::ERANGE
            } else {
                errno::EINVAL
            },
        )
    })
}

struct ControlFileSystem {
    root: Arc<ControlInode>,
}
impl FileSystem for ControlFileSystem {
    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError> {
        Ok(self.root.clone())
    }
    fn superblock(&self) -> SuperblockFacts {
        SuperblockFacts {
            type_name: "fusectl",
            read_only: false,
        }
    }
    fn statistics(&self) -> Result<FileSystemStatistics, FileSystemError> {
        Ok(FileSystemStatistics {
            magic: 0x6573_5543,
            block_size: 4096,
            blocks: 0,
            blocks_free: 0,
            blocks_available: 0,
            files: 0,
            files_free: 0,
            fsid: [self.root.filesystem as u32, 0],
            name_length: 255,
            fragment_size: 4096,
            flags: 0,
        })
    }
}
/// 标准 fusectl 类型；所有挂载共享唯一控制 superblock 与连接弱索引。
pub(super) struct ControlFileSystemType;
impl fs::mount::FileSystemType for ControlFileSystemType {
    fn name(&self) -> &'static str {
        "fusectl"
    }
    fn create(
        &self,
        request: &fs::mount::MountRequest<'_>,
    ) -> Result<fs::mount::CreatedFileSystem, FileSystemError> {
        if !mount_options::is_empty(request.options) {
            return Err(FileSystemError::InvalidOperation);
        }
        let existing = REGISTRY.lock().filesystem.clone();
        let filesystem = if let Some(existing) = existing {
            existing
        } else {
            // 分配可触发 direct reclaim，必须在索引锁外；发布时复查，保持单一 superblock。
            let root = ControlInode::new(fs::allocate_filesystem_id(), 0, 0, 0, Node::Root)?;
            let candidate = Arc::try_new(ControlFileSystem { root })
                .map_err(|_| FileSystemError::OutOfMemory)?;
            let mut registry = REGISTRY.lock();
            if registry.filesystem.is_none() {
                registry.filesystem = Some(candidate.clone());
            }
            let filesystem = registry
                .filesystem
                .as_ref()
                .expect("control filesystem initialized")
                .clone();
            drop(registry);
            filesystem
        };
        Ok(fs::mount::CreatedFileSystem {
            filesystem,
            user_namespace: crate::namespace::initial_user(),
        })
    }
}
