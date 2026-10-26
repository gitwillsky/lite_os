use alloc::{sync::Arc, vec::Vec};

use super::{
    DirectoryEntry, DirectoryRead, DirectoryVisitor, FileSystem, FileSystemError,
    FileSystemStatistics, IndexedDirectory, Inode, InodeMetadata, InodeType,
};

const SYSFS_MAGIC: u64 = 0x6265_6572;

use super::sysfs_block::{self, BlockSysNode, Parent, Place};
use super::sysfs_net::{self, NetSysNode, NetView};

#[derive(Clone, Copy)]
enum SysNode {
    Root,
    /// `/sys/class`：`block` 与 `net`。
    ClassRoot,
    /// `/sys/dev`：只有 `block`。
    DevRoot,
    /// 块设备子树（见 `sysfs_block`）。
    Block(BlockSysNode),
    /// 网络接口子树（见 `sysfs_net`）。
    Net(NetSysNode),
    Devices,
    System,
    CpuRoot,
    CpuSet(CpuSet),
    Cpu(usize),
    CpuOnline(usize),
}

#[derive(Clone, Copy)]
enum CpuSet {
    Possible,
    Present,
    Online,
}

impl SysNode {
    fn inode(self) -> u64 {
        match self {
            Self::Root => 1,
            Self::ClassRoot => 8,
            Self::DevRoot => 9,
            Self::Block(node) => node.inode(),
            Self::Net(node) => node.inode(),
            Self::Devices => 2,
            Self::System => 3,
            Self::CpuRoot => 4,
            Self::CpuSet(CpuSet::Possible) => 5,
            Self::CpuSet(CpuSet::Present) => 6,
            Self::CpuSet(CpuSet::Online) => 7,
            Self::Cpu(cpu) => 0x100 + (cpu as u64) * 2,
            Self::CpuOnline(cpu) => 0x101 + (cpu as u64) * 2,
        }
    }

    fn kind(self) -> InodeType {
        match self {
            Self::Root
            | Self::ClassRoot
            | Self::DevRoot
            | Self::Devices
            | Self::System
            | Self::CpuRoot
            | Self::Cpu(_) => InodeType::Directory,
            Self::Block(node) => node.kind(),
            Self::Net(node) => node.kind(),
            Self::CpuSet(_) | Self::CpuOnline(_) => InodeType::File,
        }
    }
}

struct SysInode {
    filesystem_id: usize,
    cpu_count: usize,
    /// 挂载时捕获的网络视图；每个 inode 共享同一份（弱引用 NET + 快照来源）。
    network: NetView,
    node: SysNode,
}

impl SysInode {
    fn new(
        filesystem_id: usize,
        cpu_count: usize,
        network: NetView,
        node: SysNode,
    ) -> Result<Arc<Self>, FileSystemError> {
        Arc::try_new(Self {
            filesystem_id,
            cpu_count,
            network,
            node,
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }

    fn decimal(output: &mut [u8], prefix: &[u8], value: usize, suffix: &[u8]) -> usize {
        output[..prefix.len()].copy_from_slice(prefix);
        let mut digits = [0u8; 20];
        let mut count = 0;
        let mut value = value;
        loop {
            digits[count] = b'0' + (value % 10) as u8;
            count += 1;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        for index in 0..count {
            output[prefix.len() + index] = digits[count - index - 1];
        }
        let end = prefix.len() + count;
        output[end..end + suffix.len()].copy_from_slice(suffix);
        end + suffix.len()
    }

    fn cpu_range(&self) -> Result<Vec<u8>, FileSystemError> {
        let mut stack = [0u8; 32];
        let length = if self.cpu_count == 1 {
            stack[..2].copy_from_slice(b"0\n");
            2
        } else {
            Self::decimal(&mut stack, b"0-", self.cpu_count - 1, b"\n")
        };
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        bytes.extend_from_slice(&stack[..length]);
        Ok(bytes)
    }

    fn contents(&self) -> Result<Vec<u8>, FileSystemError> {
        match self.node {
            SysNode::CpuSet(_) => self.cpu_range(),
            // LiteOS 不支持 CPU hotplug：能进入 userspace 的 boot 必须已启动全部 platform CPU。
            // 若这里依赖一次启动期 online 快照，后启动 CPU 会永久被 userspace 隐藏。
            SysNode::CpuOnline(_) => {
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(2)
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                bytes.extend_from_slice(b"1\n");
                Ok(bytes)
            }
            SysNode::Block(node) => sysfs_block::contents(node),
            SysNode::Net(node) => sysfs_net::contents(&self.network, node),
            _ => Err(FileSystemError::IsDirectory),
        }
    }

    fn entry(node: SysNode, name: &[u8]) -> DirectoryEntry<'_> {
        DirectoryEntry {
            inode: node.inode(),
            kind: node.kind(),
            name,
        }
    }

    fn child(&self, name: &[u8]) -> Result<SysNode, FileSystemError> {
        let parent = match self.node {
            SysNode::Root => SysNode::Root,
            SysNode::ClassRoot | SysNode::DevRoot => SysNode::Root,
            SysNode::Block(node) => match sysfs_block::parent(node)? {
                Parent::ClassRoot => SysNode::ClassRoot,
                Parent::Root => SysNode::Root,
                Parent::DevRoot => SysNode::DevRoot,
                Parent::Block(parent) => SysNode::Block(parent),
            },
            SysNode::Net(node) => match sysfs_net::parent(node) {
                sysfs_net::Parent::ClassRoot => SysNode::ClassRoot,
                sysfs_net::Parent::Net(parent) => SysNode::Net(parent),
            },
            SysNode::Devices => SysNode::Root,
            SysNode::System => SysNode::Devices,
            SysNode::CpuRoot => SysNode::System,
            SysNode::Cpu(_) => SysNode::CpuRoot,
            SysNode::CpuSet(_) | SysNode::CpuOnline(_) => {
                return Err(FileSystemError::NotDirectory);
            }
        };
        if name == b"." {
            return Ok(self.node);
        }
        if name == b".." {
            return Ok(parent);
        }
        match self.node {
            SysNode::Root if name == b"devices" => Ok(SysNode::Devices),
            SysNode::Root if name == b"class" => Ok(SysNode::ClassRoot),
            SysNode::Root if name == b"dev" => Ok(SysNode::DevRoot),
            SysNode::Root if name == b"block" => {
                Ok(SysNode::Block(BlockSysNode::Listing(Place::Block)))
            }
            SysNode::ClassRoot if name == b"block" => {
                Ok(SysNode::Block(BlockSysNode::Listing(Place::ClassBlock)))
            }
            SysNode::ClassRoot if name == b"net" => Ok(SysNode::Net(NetSysNode::Listing)),
            SysNode::Net(node) => sysfs_net::lookup(&self.network, node, name).map(SysNode::Net),
            SysNode::DevRoot if name == b"block" => {
                Ok(SysNode::Block(BlockSysNode::Listing(Place::DevBlock)))
            }
            SysNode::Block(node) => sysfs_block::lookup(node, name).map(SysNode::Block),
            SysNode::Devices if name == b"system" => Ok(SysNode::System),
            SysNode::System if name == b"cpu" => Ok(SysNode::CpuRoot),
            SysNode::CpuRoot => match name {
                b"possible" => Ok(SysNode::CpuSet(CpuSet::Possible)),
                b"present" => Ok(SysNode::CpuSet(CpuSet::Present)),
                b"online" => Ok(SysNode::CpuSet(CpuSet::Online)),
                _ => {
                    let Some(index) = name
                        .strip_prefix(b"cpu")
                        .and_then(|value| core::str::from_utf8(value).ok())
                        .and_then(|value| value.parse::<usize>().ok())
                    else {
                        return Err(FileSystemError::NotFound);
                    };
                    if index >= self.cpu_count {
                        return Err(FileSystemError::NotFound);
                    }
                    Ok(SysNode::Cpu(index))
                }
            },
            SysNode::Cpu(_) if name == b"online" => match self.node {
                SysNode::Cpu(cpu) => Ok(SysNode::CpuOnline(cpu)),
                _ => unreachable!(),
            },
            _ => Err(FileSystemError::NotFound),
        }
    }
}

impl Inode for SysInode {
    fn filesystem_id(&self) -> usize {
        self.filesystem_id
    }

    fn metadata(&self) -> Result<InodeMetadata, FileSystemError> {
        let kind = self.node.kind();
        Ok(InodeMetadata {
            filesystem: self.filesystem_id as u64,
            inode: self.node.inode(),
            kind,
            mode: if kind == InodeType::Directory {
                0o040555
            } else {
                0o100444
            },
            links: if kind == InodeType::Directory { 2 } else { 1 },
            uid: 0,
            gid: 0,
            size: if kind == InodeType::File {
                self.contents()?.len() as u64
            } else {
                0
            },
            blocks: 0,
            block_size: 4096,
            atime: 0,
            mtime: 0,
            ctime: 0,
            fine_times: Default::default(),
            device: None,
        })
    }

    fn inode_type(&self) -> InodeType {
        self.node.kind()
    }

    fn size(&self) -> u64 {
        self.contents().map_or(0, |contents| contents.len() as u64)
    }

    fn is_executable(&self) -> bool {
        false
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn read_storage(&self, offset: u64, output: &mut [u8]) -> Result<usize, FileSystemError> {
        let contents = self.contents()?;
        let offset = usize::try_from(offset).unwrap_or(usize::MAX);
        if offset >= contents.len() {
            return Ok(0);
        }
        let count = output.len().min(contents.len() - offset);
        output[..count].copy_from_slice(&contents[offset..offset + count]);
        Ok(count)
    }

    fn write_storage(
        &self,
        _offset: u64,
        _buf: &[u8],
        _identity: Option<&crate::fs::AccessIdentity>,
    ) -> Result<usize, FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }

    fn append_storage(
        &self,
        _buf: &[u8],
        _identity: Option<&crate::fs::AccessIdentity>,
    ) -> Result<(u64, usize), FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }

    fn truncate_storage(
        &self,
        _size: u64,
        _identity: Option<&crate::fs::AccessIdentity>,
    ) -> Result<(), FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }

    fn sync_storage(&self) -> Result<(), FileSystemError> {
        Ok(())
    }

    fn read_directory(
        &self,
        cursor: u64,
        visitor: &mut dyn DirectoryVisitor,
    ) -> Result<DirectoryRead, FileSystemError> {
        let parent = self.child(b"..")?;
        let mut stream = IndexedDirectory::new(cursor, visitor);
        let mut index = 0usize;
        macro_rules! emit {
            ($node:expr, $name:expr) => {{
                let entry_index = index;
                index += 1;
                if !stream.emit(entry_index, Self::entry($node, $name))? {
                    return Ok(stream.finish());
                }
            }};
        }
        emit!(self.node, b".");
        emit!(parent, b"..");
        match self.node {
            SysNode::Root => {
                emit!(SysNode::Devices, b"devices");
                emit!(SysNode::ClassRoot, b"class");
                emit!(
                    SysNode::Block(BlockSysNode::Listing(Place::Block)),
                    b"block"
                );
                emit!(SysNode::DevRoot, b"dev");
            }
            SysNode::ClassRoot => {
                emit!(
                    SysNode::Block(BlockSysNode::Listing(Place::ClassBlock)),
                    b"block"
                );
                emit!(SysNode::Net(NetSysNode::Listing), b"net");
            }
            SysNode::Net(node) => {
                for (child, name) in sysfs_net::children(&self.network, node)? {
                    emit!(SysNode::Net(child), &name);
                }
            }
            SysNode::DevRoot => emit!(
                SysNode::Block(BlockSysNode::Listing(Place::DevBlock)),
                b"block"
            ),
            SysNode::Block(node) => {
                for (child, name) in sysfs_block::children(node)? {
                    emit!(SysNode::Block(child), &name);
                }
            }
            SysNode::Devices => emit!(SysNode::System, b"system"),
            SysNode::System => emit!(SysNode::CpuRoot, b"cpu"),
            SysNode::CpuRoot => {
                emit!(SysNode::CpuSet(CpuSet::Possible), b"possible");
                emit!(SysNode::CpuSet(CpuSet::Present), b"present");
                emit!(SysNode::CpuSet(CpuSet::Online), b"online");
                let start_cpu = stream.start_index().saturating_sub(index);
                index += start_cpu;
                for cpu in start_cpu..self.cpu_count {
                    let mut name = [0u8; 32];
                    let length = Self::decimal(&mut name, b"cpu", cpu, b"");
                    emit!(SysNode::Cpu(cpu), &name[..length]);
                }
            }
            SysNode::Cpu(cpu) => {
                emit!(SysNode::CpuOnline(cpu), b"online");
            }
            SysNode::CpuSet(_) | SysNode::CpuOnline(_) => {
                return Err(FileSystemError::NotDirectory);
            }
        }
        let _ = index;
        Ok(stream.finish())
    }

    fn find_child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
        Ok(Self::new(
            self.filesystem_id,
            self.cpu_count,
            self.network.clone(),
            self.child(name)?,
        )?)
    }

    fn create(
        &self,
        _name: &[u8],
        _kind: InodeType,
        _metadata: super::CreateMetadata,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }

    fn unlink(&self, _name: &[u8], _remove_directory: bool) -> Result<(), FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }

    fn rename(
        &self,
        _old_name: &[u8],
        _new_parent_inode: u64,
        _new_name: &[u8],
        _no_replace: bool,
    ) -> Result<(), FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }
}

/// immutable DTB logical CPU topology 的只读 sysfs adapter。
pub(crate) struct SysFileSystem {
    root: Arc<SysInode>,
}

impl SysFileSystem {
    /// 创建只投影 Linux CPU topology 节点的 sysfs。
    ///
    /// # Parameters
    ///
    /// - `cpu_count`: composition root 从 CpuTopology 取得的非零 logical CPU 数。
    /// - `network`: 挂载者的网络视图，`/sys/class/net` 投影它的接口。
    ///
    /// # Returns
    ///
    /// 独立 sysfs instance；不复制任何可变 online/hotplug 或接口状态。
    pub(super) fn new(cpu_count: usize, network: NetView) -> Result<Arc<Self>, FileSystemError> {
        assert_ne!(cpu_count, 0, "sysfs requires non-empty CPU topology");
        let root = SysInode::new(
            super::allocate_filesystem_id(),
            cpu_count,
            network,
            SysNode::Root,
        )?;
        Arc::try_new(Self { root }).map_err(|_| FileSystemError::OutOfMemory)
    }
}

impl FileSystem for SysFileSystem {
    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError> {
        Ok(self.root.clone())
    }

    fn statistics(&self) -> Result<FileSystemStatistics, FileSystemError> {
        Ok(FileSystemStatistics {
            type_name: "sysfs",
            magic: SYSFS_MAGIC,
            block_size: 4096,
            blocks: 0,
            blocks_free: 0,
            blocks_available: 0,
            files: 0,
            files_free: 0,
            fsid: [self.root.filesystem_id as u32, 0],
            name_length: 255,
            fragment_size: 4096,
            flags: 1,
        })
    }
}

/// sysfs 类型：不接受选项。
pub(super) struct SysFileSystemType;

impl super::mount::FileSystemType for SysFileSystemType {
    fn name(&self) -> &'static str {
        "sysfs"
    }

    fn create(
        &self,
        request: &super::mount::MountRequest<'_>,
    ) -> Result<super::mount::CreatedFileSystem, FileSystemError> {
        if !super::mount_options::is_empty(request.options) {
            return Err(FileSystemError::InvalidOperation);
        }
        Ok(super::mount::CreatedFileSystem {
            filesystem: SysFileSystem::new(
                request.environment.cpu_count,
                NetView::new(request.environment.net_links.clone())?,
            )?,
            user_namespace: request.identity.user_namespace(),
        })
    }
}
