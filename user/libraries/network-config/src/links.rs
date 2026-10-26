//! Linux sysfs 链路快照：连接列表、状态页与通知区域共用同一状态解释，不维护可写镜像。
use crate::{CONFIG_PATH, Config};
use std::{fs, io, path::Path};

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
    /// sysfs 的 DRIVER，无设备属性时为接口名。
    pub driver: String,
    pub state: LinkState,
}

/// 按 ifindex 排序读取本机以太网接口；排除回环等非 ARPHRD_ETHER 链路。
/// 配置/目录不可读、标志或序号损坏返回 I/O 错误；枚举后尚未读取类型便删除的接口跳过。
/// 其余读取中的热拔插错误由调用方下一轮重试，不发布半份扫描结果。
/// `carrier` 在接口关闭时允许读取失败，仍按管理状态显示 Disabled。
pub fn ethernet_links() -> io::Result<Vec<Link>> {
    let root = Path::new("/sys/class/net");
    let config = Config::load(Path::new(CONFIG_PATH))?;
    let mut links = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let read =
            |name: &str| fs::read_to_string(path.join(name)).map(|text| text.trim().to_owned());
        let kind = match read("type") {
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
        let index = read("ifindex")?
            .parse()
            .map_err(|_| io::Error::other("invalid interface index"))?;
        let flags = u32::from_str_radix(read("flags")?.trim_start_matches("0x"), 16)
            .map_err(|_| io::Error::other("invalid interface flags"))?;
        let enabled = config.interface(&interface).enabled && flags & 1 != 0;
        let state = if !enabled {
            LinkState::Disabled
        } else if read("carrier")?.as_str() == "1" {
            LinkState::Connected
        } else {
            LinkState::Disconnected
        };
        let driver = read("device/uevent")
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
            driver,
            state,
        });
    }
    links.sort_by_key(|link| link.index);
    Ok(links)
}
