use alloc::{format, sync::Arc, vec, vec::Vec};

use super::{
    DirectoryEntry, FileSystem, FileSystemError, FileSystemStatistics, Inode, InodeMetadata,
    InodeType,
};

const SYS_FILESYSTEM_ID: usize = 4;
const SYSFS_MAGIC: u64 = 0x6265_6572;

#[derive(Clone, Copy)]
enum SysNode {
    Root,
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
            Self::Root | Self::Devices | Self::System | Self::CpuRoot | Self::Cpu(_) => {
                InodeType::Directory
            }
            Self::CpuSet(_) | Self::CpuOnline(_) => InodeType::File,
        }
    }
}

struct SysInode {
    cpu_count: usize,
    node: SysNode,
}

impl SysInode {
    fn new(cpu_count: usize, node: SysNode) -> Arc<Self> {
        Arc::new(Self { cpu_count, node })
    }

    fn cpu_range(&self) -> Vec<u8> {
        if self.cpu_count == 1 {
            b"0\n".to_vec()
        } else {
            format!("0-{}\n", self.cpu_count - 1).into_bytes()
        }
    }

    fn contents(&self) -> Result<Vec<u8>, FileSystemError> {
        match self.node {
            SysNode::CpuSet(_) => Ok(self.cpu_range()),
            // LiteOS 不支持 CPU hotplug：能进入 userspace 的 boot 必须已启动全部 DTB hart。
            // 若这里依赖一次启动期 online 快照，后启动 hart 会永久被 userspace 隐藏。
            SysNode::CpuOnline(_) => Ok(b"1\n".to_vec()),
            _ => Err(FileSystemError::IsDirectory),
        }
    }

    fn entry(node: SysNode, name: &[u8]) -> DirectoryEntry {
        DirectoryEntry {
            inode: node.inode(),
            kind: node.kind(),
            name: name.to_vec(),
        }
    }

    fn child(&self, name: &[u8]) -> Result<SysNode, FileSystemError> {
        let parent = match self.node {
            SysNode::Root => SysNode::Root,
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
        SYS_FILESYSTEM_ID
    }

    fn metadata(&self) -> Result<InodeMetadata, FileSystemError> {
        let kind = self.node.kind();
        Ok(InodeMetadata {
            filesystem: SYS_FILESYSTEM_ID as u64,
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

    fn write_storage(&self, _offset: u64, _buf: &[u8]) -> Result<usize, FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }

    fn append_storage(&self, _buf: &[u8]) -> Result<(u64, usize), FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }

    fn truncate_storage(&self, _size: u64) -> Result<(), FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }

    fn sync_storage(&self) -> Result<(), FileSystemError> {
        Ok(())
    }

    fn list(&self) -> Result<Vec<DirectoryEntry>, FileSystemError> {
        let parent = self.child(b"..")?;
        let mut entries = vec![Self::entry(self.node, b"."), Self::entry(parent, b"..")];
        match self.node {
            SysNode::Root => entries.push(Self::entry(SysNode::Devices, b"devices")),
            SysNode::Devices => entries.push(Self::entry(SysNode::System, b"system")),
            SysNode::System => entries.push(Self::entry(SysNode::CpuRoot, b"cpu")),
            SysNode::CpuRoot => {
                entries.push(Self::entry(SysNode::CpuSet(CpuSet::Possible), b"possible"));
                entries.push(Self::entry(SysNode::CpuSet(CpuSet::Present), b"present"));
                entries.push(Self::entry(SysNode::CpuSet(CpuSet::Online), b"online"));
                for cpu in 0..self.cpu_count {
                    entries.push(Self::entry(
                        SysNode::Cpu(cpu),
                        format!("cpu{cpu}").as_bytes(),
                    ));
                }
            }
            SysNode::Cpu(cpu) => {
                entries.push(Self::entry(SysNode::CpuOnline(cpu), b"online"));
            }
            SysNode::CpuSet(_) | SysNode::CpuOnline(_) => {
                return Err(FileSystemError::NotDirectory);
            }
        }
        Ok(entries)
    }

    fn find_child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
        Ok(Self::new(self.cpu_count, self.child(name)?))
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

/// @description immutable DTB logical CPU topology 的只读 sysfs adapter。
pub(crate) struct SysFileSystem {
    root: Arc<SysInode>,
}

impl SysFileSystem {
    /// @description 创建只投影 Linux CPU topology 节点的 sysfs。
    ///
    /// @param cpu_count composition root 从 HartTopology 取得的非零 logical CPU 数。
    /// @return 独立 sysfs instance；不复制任何可变 online/hotplug 状态。
    pub(crate) fn new(cpu_count: usize) -> Arc<Self> {
        assert_ne!(cpu_count, 0, "sysfs requires non-empty CPU topology");
        Arc::new(Self {
            root: SysInode::new(cpu_count, SysNode::Root),
        })
    }
}

impl FileSystem for SysFileSystem {
    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError> {
        Ok(self.root.clone())
    }

    fn statistics(&self) -> FileSystemStatistics {
        FileSystemStatistics {
            type_name: "sysfs",
            magic: SYSFS_MAGIC,
            block_size: 4096,
            blocks: 0,
            blocks_free: 0,
            blocks_available: 0,
            files: 0,
            files_free: 0,
            fsid: [SYS_FILESYSTEM_ID as u32, 0],
            name_length: 255,
            fragment_size: 4096,
            flags: 1,
        }
    }
}
