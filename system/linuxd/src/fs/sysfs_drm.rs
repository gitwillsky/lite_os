//! 标准 DRM sysfs 设备发现。只投影注册 adapter 的真实 MMIO/VirtIO identity，不从 userspace 路径猜测硬件。
use super::{FileSystemError, InodeType, try_format_bytes};
use alloc::vec::Vec;
#[derive(Clone, Copy)]
pub(super) enum Node {
    Class,
    Char,
    Platform,
    Bus,
    PlatformBus,
    VirtioBus,
    Transport,
    Device,
    Drm,
    Card(bool),
    CardLink(bool),
    DeviceLink,
    VirtioSubsystem,
    PlatformSubsystem,
    Dev(bool),
    Uevent,
}
pub(super) enum Parent {
    Root,
    Class,
    Dev,
    Devices,
    Node(Node),
}
/// 设备领域注入的只读身份来源；sysfs 不反向查询 DRM/driver 注册表，也不复制可变设备状态。
pub(crate) trait GraphicsSysfsSource: Send + Sync {
    /// 实际 MMIO 地址与 VirtIO vendor；非 MMIO 设备为 None。
    fn identity(&self) -> Option<(u64, u32)>;
}
// OWNER: 启动装配仅发布一次实际设备 source；不拥有 renderer/context/资源表。
// 缺少来源时 DRM sysfs 为空，libdrm 无法把 card/render 节点归到同一设备；重复发布为初始化错误。
static SOURCE: spin::Once<alloc::sync::Arc<dyn GraphicsSysfsSource>> = spin::Once::new();
pub(crate) fn register_source(
    source: alloc::sync::Arc<dyn GraphicsSysfsSource>,
) -> Result<(), FileSystemError> {
    if SOURCE.get().is_some() {
        return Err(FileSystemError::AlreadyExists);
    }
    SOURCE.call_once(|| source);
    Ok(())
}
fn identity() -> Result<(u64, u32), FileSystemError> {
    SOURCE
        .get()
        .and_then(|source| source.identity())
        .ok_or(FileSystemError::NotFound)
}

impl Node {
    pub(super) fn inode(self) -> u64 {
        0x400000
            + match self {
                Self::Class => 0,
                Self::Char => 1,
                Self::Platform => 2,
                Self::Bus => 3,
                Self::PlatformBus => 4,
                Self::VirtioBus => 5,
                Self::Transport => 6,
                Self::Device => 7,
                Self::Drm => 8,
                Self::Card(render) => 9 + u64::from(render),
                Self::CardLink(render) => 11 + u64::from(render),
                Self::DeviceLink => 13,
                Self::VirtioSubsystem => 14,
                Self::PlatformSubsystem => 15,
                Self::Dev(render) => 16 + u64::from(render),
                Self::Uevent => 18,
            }
    }
    pub(super) fn kind(self) -> InodeType {
        match self {
            Self::CardLink(_)
            | Self::DeviceLink
            | Self::VirtioSubsystem
            | Self::PlatformSubsystem => InodeType::SymLink,
            Self::Dev(_) | Self::Uevent => InodeType::File,
            _ => InodeType::Directory,
        }
    }
}
pub(super) fn parent(node: Node) -> Parent {
    use Node::*;
    match node {
        Class => Parent::Class,
        Char => Parent::Dev,
        Platform => Parent::Devices,
        Bus => Parent::Root,
        PlatformBus | VirtioBus => Parent::Node(Bus),
        Transport => Parent::Node(Platform),
        Device | PlatformSubsystem => Parent::Node(Transport),
        Drm | VirtioSubsystem | Uevent => Parent::Node(Device),
        Card(_) => Parent::Node(Drm),
        Dev(render) => Parent::Node(Card(render)),
        DeviceLink => Parent::Node(Card(false)),
        CardLink(_) => Parent::Node(Class),
    }
}
pub(super) fn lookup(node: Node, name: &[u8]) -> Result<Node, FileSystemError> {
    use Node::*;
    identity()?;
    let result = match (node, name) {
        (Class | Drm, b"card0") => {
            if matches!(node, Class) {
                CardLink(false)
            } else {
                Card(false)
            }
        }
        (Class | Drm, b"renderD128") => {
            if matches!(node, Class) {
                CardLink(true)
            } else {
                Card(true)
            }
        }
        (Char, b"226:0") => CardLink(false),
        (Char, b"226:128") => CardLink(true),
        (Bus, b"platform") => PlatformBus,
        (Bus, b"virtio") => VirtioBus,
        (Transport, b"virtio-gpu") => Device,
        (Transport, b"subsystem") => PlatformSubsystem,
        (Device, b"drm") => Drm,
        (Device, b"subsystem") => VirtioSubsystem,
        (Device, b"uevent") => Uevent,
        (Card(_), b"device") => DeviceLink,
        (Card(render), b"dev") => Dev(render),
        (Platform, _) if name == transport_name()?.as_slice() => Transport,
        _ => return Err(FileSystemError::NotFound),
    };
    Ok(result)
}
fn transport_name() -> Result<Vec<u8>, FileSystemError> {
    let (address, _) = identity()?;
    try_format_bytes(format_args!("{address:x}.virtio_mmio"))
}
pub(super) fn children(node: Node) -> Result<Vec<(Node, Vec<u8>)>, FileSystemError> {
    use Node::*;
    let mut children = Vec::new();
    if identity().is_err() {
        return Ok(children);
    }
    let entries: &[(Node, &[u8])] = match node {
        Class => &[(CardLink(false), b"card0"), (CardLink(true), b"renderD128")],
        Char => &[(CardLink(false), b"226:0"), (CardLink(true), b"226:128")],
        Bus => &[(PlatformBus, b"platform"), (VirtioBus, b"virtio")],
        Transport => &[(Device, b"virtio-gpu"), (PlatformSubsystem, b"subsystem")],
        Device => &[
            (Drm, b"drm"),
            (VirtioSubsystem, b"subsystem"),
            (Uevent, b"uevent"),
        ],
        Drm => &[(Card(false), b"card0"), (Card(true), b"renderD128")],
        Card(false) => &[(DeviceLink, b"device"), (Dev(false), b"dev")],
        Card(true) => &[(DeviceLink, b"device"), (Dev(true), b"dev")],
        _ => &[],
    };
    children
        .try_reserve_exact(entries.len() + usize::from(matches!(node, Platform)))
        .map_err(|_| FileSystemError::OutOfMemory)?;
    for (node, name) in entries {
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(name.len())
            .map_err(|_| FileSystemError::OutOfMemory)?;
        owned.extend_from_slice(name);
        children.push((*node, owned));
    }
    if matches!(node, Platform) {
        children.push((Transport, transport_name()?));
    }
    Ok(children)
}
pub(super) fn contents(node: Node) -> Result<Vec<u8>, FileSystemError> {
    match node {
        Node::Dev(render) => {
            try_format_bytes(format_args!("226:{}\n", if render { 128 } else { 0 }))
        }
        Node::Uevent => {
            let (_, vendor) = identity()?;
            try_format_bytes(format_args!(
                "DRIVER=virtio_gpu\nMODALIAS=virtio:d00000010v{vendor:08X}\n"
            ))
        }
        _ => Err(FileSystemError::InvalidOperation),
    }
}
pub(super) fn read_link(node: Node) -> Result<Vec<u8>, FileSystemError> {
    match node {
        Node::CardLink(render) => {
            let name = transport_name()?;
            try_format_bytes(format_args!(
                "/sys/devices/platform/{}/virtio-gpu/drm/{}",
                core::str::from_utf8(&name).map_err(|_| FileSystemError::InvalidOperation)?,
                if render { "renderD128" } else { "card0" }
            ))
        }
        Node::DeviceLink => try_format_bytes(format_args!("../..")),
        Node::VirtioSubsystem => try_format_bytes(format_args!("/sys/bus/virtio")),
        Node::PlatformSubsystem => try_format_bytes(format_args!("/sys/bus/platform")),
        _ => Err(FileSystemError::InvalidOperation),
    }
}
