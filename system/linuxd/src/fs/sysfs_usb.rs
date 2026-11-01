//! 标准 USB sysfs 发现与 remove 属性；sysfs 不复制在线状态或直接操作 host 寄存器。
use super::{FileSystemError, InodeType, try_format_bytes};
use crate::devices::model::{UsbDevice, UsbRemoveError};
use alloc::{sync::Arc, vec::Vec};

/// USB 设备领域注入的发现来源；稳定 index 标识一次连接。
pub(crate) trait UsbSysfsSource: Send + Sync {
    /// 已发布连接数，包含可能被旧 inode 引用的断开连接。
    fn count(&self) -> usize;
    /// 返回一次连接的 adapter，在线状态由 adapter owner 裁决。
    fn device(&self, index: usize) -> Option<Arc<dyn UsbDevice>>;
}
// OWNER: composition root 只装配一次；source 只有注册表访问权，不拥有 USB 连接状态。
// 缺少来源时目录为空；重复发布必须失败，避免旧 inode 被重新绑定到另一份注册表。
static SOURCE: spin::Once<Arc<dyn UsbSysfsSource>> = spin::Once::new();
/// 注册唯一 USB sysfs 来源；重复装配返回 AlreadyExists。
pub(crate) fn register_source(source: Arc<dyn UsbSysfsSource>) -> Result<(), FileSystemError> {
    if SOURCE.get().is_some() {
        return Err(FileSystemError::AlreadyExists);
    }
    SOURCE.call_once(|| source);
    Ok(())
}
#[derive(Clone, Copy)]
pub(super) enum Node {
    Bus,
    Devices,
    Device(usize),
    Attribute(usize, Attribute),
}
#[derive(Clone, Copy)]
pub(super) enum Attribute {
    Vendor,
    Product,
    Name,
    Bus,
    Port,
    Remove,
}
const ATTRIBUTES: &[(Attribute, &[u8])] = &[
    (Attribute::Vendor, b"idVendor"),
    (Attribute::Product, b"idProduct"),
    (Attribute::Name, b"product"),
    (Attribute::Bus, b"busnum"),
    (Attribute::Port, b"devpath"),
    (Attribute::Remove, b"remove"),
];
fn device(index: usize) -> Result<Arc<dyn UsbDevice>, FileSystemError> {
    SOURCE
        .get()
        .and_then(|source| source.device(index))
        .filter(|device| device.connected())
        .ok_or(FileSystemError::NotFound)
}
impl Node {
    pub(super) fn inode(self) -> u64 {
        0x600000
            + match self {
                Self::Bus => 0,
                Self::Devices => 1,
                Self::Device(index) => 2 + index as u64 * 8,
                Self::Attribute(index, attribute) => 3 + index as u64 * 8 + attribute as u64,
            }
    }
    pub(super) fn kind(self) -> InodeType {
        if matches!(self, Self::Attribute(..)) {
            InodeType::File
        } else {
            InodeType::Directory
        }
    }
    pub(super) fn writable(self) -> bool {
        matches!(self, Self::Attribute(_, Attribute::Remove))
    }
}
pub(super) fn parent(node: Node) -> Option<Node> {
    match node {
        Node::Bus => None,
        Node::Devices => Some(Node::Bus),
        Node::Device(_) => Some(Node::Devices),
        Node::Attribute(index, _) => Some(Node::Device(index)),
    }
}
pub(super) fn children(node: Node) -> Result<Vec<(Node, Vec<u8>)>, FileSystemError> {
    let mut result = Vec::new();
    match node {
        Node::Bus => {
            result
                .try_reserve(1)
                .map_err(|_| FileSystemError::OutOfMemory)?;
            result.push((Node::Devices, try_format_bytes(format_args!("devices"))?));
        }
        Node::Devices => {
            if let Some(source) = SOURCE.get() {
                for index in 0..source.count() {
                    let Ok(device) = device(index) else { continue };
                    let id = device.identity();
                    result
                        .try_reserve(1)
                        .map_err(|_| FileSystemError::OutOfMemory)?;
                    result.push((
                        Node::Device(index),
                        try_format_bytes(format_args!("{}-{}", id.bus, id.port))?,
                    ));
                }
            }
        }
        Node::Device(index) => {
            device(index)?;
            for &(attribute, name) in ATTRIBUTES {
                result
                    .try_reserve(1)
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                let mut text = Vec::new();
                text.try_reserve_exact(name.len())
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                text.extend_from_slice(name);
                result.push((Node::Attribute(index, attribute), text));
            }
        }
        Node::Attribute(..) => return Err(FileSystemError::NotDirectory),
    }
    Ok(result)
}
pub(super) fn lookup(node: Node, name: &[u8]) -> Result<Node, FileSystemError> {
    children(node)?
        .into_iter()
        .find_map(|(node, child)| (child == name).then_some(node))
        .ok_or(FileSystemError::NotFound)
}
pub(super) fn contents(node: Node) -> Result<Vec<u8>, FileSystemError> {
    let Node::Attribute(index, attribute) = node else {
        return Err(FileSystemError::IsDirectory);
    };
    let device = device(index)?;
    let id = device.identity();
    match attribute {
        Attribute::Vendor => try_format_bytes(format_args!("{:04x}\n", id.vendor)),
        Attribute::Product => try_format_bytes(format_args!("{:04x}\n", id.product)),
        Attribute::Name => try_format_bytes(format_args!("{}\n", device.name())),
        Attribute::Bus => try_format_bytes(format_args!("{}\n", id.bus)),
        Attribute::Port => try_format_bytes(format_args!("{}\n", id.port)),
        Attribute::Remove => try_format_bytes(format_args!("")),
    }
}
pub(super) fn open(node: Node) -> Result<Arc<dyn super::chardev::DeviceFile>, FileSystemError> {
    let Node::Attribute(index, Attribute::Remove) = node else {
        return Err(FileSystemError::ReadOnly);
    };
    Arc::try_new(RemoveFile(device(index)?))
        .map(|file| file as Arc<dyn super::chardev::DeviceFile>)
        .map_err(|_| FileSystemError::OutOfMemory)
}
// OWNER: 这次 open 直接持有连接 adapter，而不是重新按 bus/port 查找；旧 fd 不能停止后来重插的设备。
struct RemoveFile(Arc<dyn UsbDevice>);
impl super::chardev::DeviceFile for RemoveFile {
    fn positioned(&self) -> Option<&dyn super::chardev::PositionedFile> {
        Some(self)
    }
    fn poll(&self, events: i16) -> i16 {
        events & 4
    }
}
impl super::chardev::PositionedFile for RemoveFile {
    fn read_at(
        &self,
        _: &mut dyn super::chardev::UserOutput,
        _: u64,
    ) -> Result<(), super::chardev::DeviceError> {
        Ok(())
    }
    fn write_at(
        &self,
        input: &mut dyn super::chardev::UserInput,
        _: u64,
        _: &crate::namespace::NamespaceAuthority,
    ) -> Result<(), super::chardev::DeviceError> {
        use super::chardev::DeviceError;
        use syscall_abi::errno;
        let count = input.remaining().min(crate::memory::PAGE_SIZE);
        if count == 0 {
            return Ok(());
        }
        // Linux sysfs store 最多接收一页；remove 忽略内容，但副作用前必须完成 user-copy。
        // 冷路径使用有界 stack，避免停止硬件途中才因分配或用户指针失败。
        let mut bytes = [0; crate::memory::PAGE_SIZE];
        input
            .copy(&mut bytes[..count])
            .map_err(|_| DeviceError::Errno(errno::EFAULT))?;
        self.0.remove().map_err(|error| {
            DeviceError::Errno(match error {
                UsbRemoveError::Disconnected => errno::ENODEV,
                UsbRemoveError::Device => errno::EIO,
            })
        })?;
        input.consume(count);
        Ok(())
    }
    fn seek(
        &self,
        current: u64,
        offset: i64,
        whence: u32,
    ) -> Result<u64, super::chardev::DeviceError> {
        use super::chardev::DeviceError;
        let base = match whence {
            0 => 0,
            1 => current,
            _ => return Err(DeviceError::Errno(syscall_abi::errno::EINVAL)),
        };
        let next = i128::from(base) + i128::from(offset);
        if !(0..=i128::from(i64::MAX)).contains(&next) {
            return Err(DeviceError::Errno(syscall_abi::errno::EINVAL));
        }
        Ok(next as u64)
    }
}
