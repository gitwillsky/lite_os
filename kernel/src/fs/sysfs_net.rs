//! sysfs 的网络接口子树：`/sys/class/net/<接口>`。
//!
//! 内容在每次读取时从挂载者网络命名空间的实际接口表取快照，不缓存；接口以 ifindex 为稳定身份。属性文本与
//! Linux `net/core/net-sysfs.c` 相同：`ifindex`、`type`、`flags`、`mtu`、`address`、`operstate`、`carrier`、`uevent`；
//! 有驱动的接口另有 `device/uevent`（`DRIVER=`），供 ip、udhcpc、NetworkManager 一类工具与资源管理器枚举。
//!
//! 与 Linux 的差别：`/sys/class/net/<接口>` 与 `device` 是目录，而不是指向 `/sys/devices/...` 的符号链接
//! （sysfs 在这里没有符号链接节点，与 `/sys/dev/block` 的处理一致）；`carrier` 跟随管理状态，
//! 因为网络设备 seam 不报告物理链路状态。

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};

use super::{FileSystemError, InodeType, try_format_bytes};

/// sysfs 网络子树的数据来源：调用者的网络命名空间与它的实际接口表；由 task 层实现、composition root 注入。
pub(crate) trait NetLinkSource: Send + Sync {
    /// 挂载 sysfs 的任务所在的 NET。
    fn current_net_namespace(&self) -> Result<Arc<crate::socket::NetNamespace>, FileSystemError>;
    /// `target` 的全部接口；与 `SIOCGIFFLAGS`、rtnetlink 共用同一份接口快照。
    fn link_snapshot(
        &self,
        target: &crate::socket::NetNamespace,
    ) -> Result<Vec<NetLinkSnapshot>, FileSystemError>;
}

/// 一个接口的只读窄投影；由 [`NetLinkSource::link_snapshot`] 从实际接口 owner 读取。
#[derive(Clone, Copy)]
pub(crate) struct NetLinkSnapshot {
    pub(crate) index: u32,
    pub(crate) name: [u8; 16],
    pub(crate) loopback: bool,
    /// `IFF_*`，与 `SIOCGIFFLAGS`、rtnetlink 共用同一份计算。
    pub(crate) flags: u16,
    pub(crate) mtu: i32,
    pub(crate) mac: [u8; 6],
    pub(crate) up: bool,
    pub(crate) driver: Option<&'static str>,
}

impl NetLinkSnapshot {
    fn name(&self) -> &[u8] {
        let end = self.name.iter().position(|byte| *byte == 0).unwrap_or(16);
        &self.name[..end]
    }
}

/// 挂载时捕获的网络视图：挂载者的 NET 与快照来源（Linux sysfs 按挂载者 netns 标记 `class/net`）。
#[derive(Clone)]
pub(super) struct NetView {
    // OWNER: sysfs 实例只弱引用挂载者 NET；NET 销毁后列表为空，不因挂载而延长命名空间寿命。
    // FAILURE: 缺少它只能列出某个固定 NET，容器内挂载的 sysfs 会看到宿主接口。
    namespace: Weak<crate::socket::NetNamespace>,
    source: Arc<dyn NetLinkSource>,
}

impl NetView {
    pub(super) fn new(source: Arc<dyn NetLinkSource>) -> Result<Self, FileSystemError> {
        let namespace = Arc::downgrade(&source.current_net_namespace()?);
        Ok(Self { namespace, source })
    }

    fn links(&self) -> Result<Vec<NetLinkSnapshot>, FileSystemError> {
        match self.namespace.upgrade() {
            Some(namespace) => self.source.link_snapshot(&namespace),
            None => Ok(Vec::new()),
        }
    }

    fn link(&self, index: u32) -> Result<NetLinkSnapshot, FileSystemError> {
        self.links()?
            .into_iter()
            .find(|link| link.index == index)
            .ok_or(FileSystemError::NotFound)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Attr {
    Ifindex,
    Type,
    Flags,
    Mtu,
    Address,
    Operstate,
    Carrier,
    Uevent,
}

const ATTRS: [(Attr, &[u8]); 8] = [
    (Attr::Ifindex, b"ifindex"),
    (Attr::Type, b"type"),
    (Attr::Flags, b"flags"),
    (Attr::Mtu, b"mtu"),
    (Attr::Address, b"address"),
    (Attr::Operstate, b"operstate"),
    (Attr::Carrier, b"carrier"),
    (Attr::Uevent, b"uevent"),
];

/// 网络子树中的一个节点；`u32` 是接口的 ifindex。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum NetSysNode {
    /// `/sys/class/net`。
    Listing,
    Entry(u32),
    Attr(u32, Attr),
    /// `/sys/class/net/<接口>/device`。
    Device(u32),
    /// `device/uevent`。
    DeviceUevent(u32),
}

/// 父目录：`/sys/class`，或子树内的另一个节点。
pub(super) enum Parent {
    ClassRoot,
    Net(NetSysNode),
}

impl NetSysNode {
    pub(super) fn inode(self) -> u64 {
        const BASE: u64 = 2 << 40;
        let index = |index: u32| u64::from(index) << 8;
        match self {
            Self::Listing => 0x30,
            Self::Entry(interface) => BASE | index(interface),
            Self::Attr(interface, attr) => BASE | index(interface) | (attr as u64 + 1),
            Self::Device(interface) => BASE | index(interface) | 0x80,
            Self::DeviceUevent(interface) => BASE | index(interface) | 0x81,
        }
    }

    pub(super) fn kind(self) -> InodeType {
        match self {
            Self::Listing | Self::Entry(_) | Self::Device(_) => InodeType::Directory,
            Self::Attr(..) | Self::DeviceUevent(_) => InodeType::File,
        }
    }
}

fn owned(name: &[u8]) -> Result<Vec<u8>, FileSystemError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(name.len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    bytes.extend_from_slice(name);
    Ok(bytes)
}

/// `node` 的子节点（目录项）；文件没有子节点。
pub(super) fn children(
    view: &NetView,
    node: NetSysNode,
) -> Result<Vec<(NetSysNode, Vec<u8>)>, FileSystemError> {
    let mut entries = Vec::new();
    let mut push = |child: NetSysNode, name: Vec<u8>| {
        entries
            .try_reserve(1)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        entries.push((child, name));
        Ok::<(), FileSystemError>(())
    };
    match node {
        NetSysNode::Listing => {
            for link in view.links()? {
                push(NetSysNode::Entry(link.index), owned(link.name())?)?;
            }
        }
        NetSysNode::Entry(index) => {
            let link = view.link(index)?;
            for (attr, name) in ATTRS {
                push(NetSysNode::Attr(index, attr), owned(name)?)?;
            }
            if link.driver.is_some() {
                push(NetSysNode::Device(index), owned(b"device")?)?;
            }
        }
        NetSysNode::Device(index) => {
            view.link(index)?;
            push(NetSysNode::DeviceUevent(index), owned(b"uevent")?)?;
        }
        NetSysNode::Attr(..) | NetSysNode::DeviceUevent(_) => {
            return Err(FileSystemError::NotDirectory);
        }
    }
    Ok(entries)
}

/// 在 `node` 目录下按名字查找子节点。
pub(super) fn lookup(
    view: &NetView,
    node: NetSysNode,
    name: &[u8],
) -> Result<NetSysNode, FileSystemError> {
    children(view, node)?
        .into_iter()
        .find(|(_, candidate)| candidate == name)
        .map(|(child, _)| child)
        .ok_or(FileSystemError::NotFound)
}

pub(super) fn parent(node: NetSysNode) -> Parent {
    match node {
        NetSysNode::Listing => Parent::ClassRoot,
        NetSysNode::Entry(_) => Parent::Net(NetSysNode::Listing),
        NetSysNode::Attr(index, _) | NetSysNode::Device(index) => {
            Parent::Net(NetSysNode::Entry(index))
        }
        NetSysNode::DeviceUevent(index) => Parent::Net(NetSysNode::Device(index)),
    }
}

/// 属性文件的内容（Linux `net-sysfs.c` 的文本格式）。
///
/// # Errors
///
/// 接口已不存在为 NotFound；管理关闭的接口读 `carrier` 为 InvalidOperation（Linux 返回 EINVAL）。
pub(super) fn contents(view: &NetView, node: NetSysNode) -> Result<Vec<u8>, FileSystemError> {
    let (index, attr) = match node {
        NetSysNode::Attr(index, attr) => (index, Some(attr)),
        NetSysNode::DeviceUevent(index) => (index, None),
        _ => return Err(FileSystemError::IsDirectory),
    };
    let link = view.link(index)?;
    let name = core::str::from_utf8(link.name()).unwrap_or("?");
    let Some(attr) = attr else {
        return try_format_bytes(format_args!("DRIVER={}\n", link.driver.unwrap_or("")));
    };
    match attr {
        Attr::Ifindex => try_format_bytes(format_args!("{}\n", link.index)),
        // ARPHRD_LOOPBACK / ARPHRD_ETHER。
        Attr::Type => try_format_bytes(format_args!("{}\n", if link.loopback { 772 } else { 1 })),
        Attr::Flags => try_format_bytes(format_args!("{:#x}\n", link.flags)),
        Attr::Mtu => try_format_bytes(format_args!("{}\n", link.mtu)),
        Attr::Address => {
            let mac = link.mac;
            try_format_bytes(format_args!(
                "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\n",
                mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
            ))
        }
        // RFC 2863 operstate：loopback 不报告链路，Linux 显示 unknown。
        Attr::Operstate => try_format_bytes(format_args!(
            "{}\n",
            match (link.up, link.loopback) {
                (false, _) => "down",
                (true, true) => "unknown",
                (true, false) => "up",
            }
        )),
        Attr::Carrier if link.up => try_format_bytes(format_args!("1\n")),
        Attr::Carrier => Err(FileSystemError::InvalidOperation),
        Attr::Uevent => {
            try_format_bytes(format_args!("INTERFACE={name}\nIFINDEX={}\n", link.index))
        }
    }
}
