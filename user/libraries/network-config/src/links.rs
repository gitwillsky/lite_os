//! Linux sysfs 链路快照：连接列表、状态页与通知区域共用同一状态解释，不维护可写镜像。
use std::{collections::BTreeSet, fs, io, path::Path};

const NET_CLASS: &str = "/sys/class/net";

/// 接口的管理状态与物理载波；Connected 仅表示链路建立，不保证可以访问互联网。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkState {
    Disabled,
    Disconnected,
    Connected,
}

/// 按 Linux 接口序号标识的以太网链路快照；只由扫描结果构造。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub index: u32,
    pub interface: String,
    /// 系统设置中的连接名，或按当前以太网接口顺序生成的默认名。
    pub name: String,
    /// sysfs 的 DRIVER，无设备属性时为接口名。
    pub driver: String,
    pub state: LinkState,
}

fn read(path: &Path, attr: &str) -> io::Result<String> {
    fs::read_to_string(path.join(attr)).map(|text| text.trim().to_owned())
}

/// 指定接口当前的管理状态和载波；`settings` 可复用调用者的 netcfg 快照。
/// 接口不存在、sysfs 不可读或返回非法状态时返回 I/O 错误。
pub fn link_state(interface: &str, settings: Option<&crate::Snapshot>) -> io::Result<LinkState> {
    let path = Path::new(NET_CLASS).join(interface);
    let flags = u32::from_str_radix(read(&path, "flags")?.trim_start_matches("0x"), 16)
        .map_err(|_| io::Error::other("invalid interface flags"))?;
    let disabled = settings.is_some_and(|settings| {
        settings.interfaces.iter().any(|state| {
            state.name == interface.as_bytes()
                && state.config.method == netcfg_protocol::Method::Disabled
        })
    });
    if disabled || flags & 1 == 0 {
        return Ok(LinkState::Disabled);
    }
    match read(&path, "carrier")?.as_str() {
        "0" => Ok(LinkState::Disconnected),
        "1" => Ok(LinkState::Connected),
        _ => Err(io::Error::other("invalid interface carrier")),
    }
}

/// 按 ifindex 排序读取本机以太网接口；排除回环等非 ARPHRD_ETHER 链路。
/// 目录不可读、标志或序号损坏返回 I/O 错误；枚举后尚未读取类型便删除的接口跳过。
/// 用户设置中的“禁用”来自 netcfg；状态端点暂不可用时只按接口的管理状态判断。
/// 其余读取中的热拔插错误由调用方下一轮重试，不发布半份扫描结果。
/// `carrier` 在接口关闭时允许读取失败，仍按管理状态显示 Disabled。
pub fn ethernet_links() -> io::Result<Vec<Link>> {
    let root = Path::new(NET_CLASS);
    let settings = crate::snapshot().ok();
    let mut links = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let kind = match read(&path, "type") {
            Ok(kind) => kind,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if kind != "1" {
            continue;
        }
        let interface = entry
            .file_name()
            .into_string()
            .map_err(|_| io::Error::other("invalid interface name"))?;
        let index = read(&path, "ifindex")?
            .parse()
            .map_err(|_| io::Error::other("invalid interface index"))?;
        let state = link_state(&interface, settings.as_ref())?;
        let name = settings
            .as_ref()
            .and_then(|snapshot| {
                snapshot
                    .interfaces
                    .iter()
                    .find(|entry| entry.name == interface.as_bytes())
            })
            .and_then(|entry| std::str::from_utf8(&entry.connection_name).ok())
            .unwrap_or_default()
            .to_owned();
        let driver = read(&path, "device/uevent")
            .unwrap_or_default()
            .lines()
            .find_map(|line| {
                line.strip_prefix("DRIVER=")
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| interface.clone());
        links.push(Link {
            index,
            interface,
            name,
            driver,
            state,
        });
    }
    links.sort_by_key(|link| link.index);
    let mut names: BTreeSet<_> = links
        .iter()
        .filter(|link| !link.name.is_empty())
        .map(|link| link.name.to_lowercase())
        .collect();
    for (index, link) in links.iter_mut().enumerate() {
        if !link.name.is_empty() {
            continue;
        }
        let mut number = index + 1;
        // 热插入的接口可以占到某个默认序号；跳过已被用户使用的名称。
        loop {
            let name = if number == 1 {
                "本地连接".to_owned()
            } else {
                format!("本地连接 {number}")
            };
            number += 1;
            if names.insert(name.clone()) {
                link.name = name;
                break;
            }
        }
    }
    Ok(links)
}

/// 改变已安装接口的系统显示名，保留 Linux 接口及网络设置。
///
/// # Errors
///
/// 连接已移除、名称与现有连接重复，或 netcfg 拒绝请求。
pub fn rename_connection(interface: &str, name: &str) -> io::Result<()> {
    netcfg_protocol::connection_name(name.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "连接名称无效。请输入非空名称，避免使用保留字符。",
        )
    })?;
    let links = ethernet_links()?;
    if !links.iter().any(|link| link.interface == interface) {
        return Err(io::Error::new(io::ErrorKind::NotFound, "网络连接已移除"));
    }
    let folded = name.to_lowercase();
    if links
        .iter()
        .any(|link| link.interface != interface && link.name.to_lowercase() == folded)
    {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "具有指定名称的连接已经存在。请指定不同的名称。",
        ));
    }
    crate::settings::rename(interface, name)
}
