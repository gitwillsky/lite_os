//! 资源管理器的位置与条目：真实目录加上“我的电脑”“控制面板”“网上邻居”“网络和拨号连接”等虚拟文件夹。
//! 每个位置的详细资料列与文案取自 Windows 2000 SP4 简中 shell32/netshell 字符串表。
use crate::fileops;
use std::{
    cmp::Ordering,
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

/// 单个目录最多列出的条目数；超出的部分被丢弃并在状态栏提示。
pub const MAX_ENTRIES: usize = 4096;

/// 资源管理器可以显示的位置。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Location {
    /// 命名空间根“桌面”：我的文档、我的电脑、网上邻居、回收站，加上 `~/Desktop` 中的文件。
    Desktop,
    MyComputer,
    ControlPanel,
    /// 网上邻居：只有“整个网络”；系统没有 SMB 与网络邻居发现协议。
    NetworkPlaces,
    /// 整个网络：没有网络提供程序时为空文件夹。
    EntireNetwork,
    /// 网络和拨号连接：本机的局域网接口（`/sys/class/net`）。
    NetworkConnections,
    Directory(PathBuf),
}

/// 条目图标；与 `/usr/share/liteos/ui/<name>-{16,32}.bmp` 一一对应。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Desktop,
    MyComputer,
    Folder,
    File,
    Text,
    Application,
    System,
    Disk,
    OpticalDisc,
    ControlPanel,
    MyDocuments,
    RecycleBin,
    DateTime,
    Sound,
    DisplaySettings,
    Users,
    NetworkPlaces,
    EntireNetwork,
    Network,
    LocalConnection,
    DisconnectedConnection,
    DeviceManager,
}

impl Icon {
    pub fn file_stem(self) -> &'static str {
        match self {
            Icon::Desktop => "desktop",
            Icon::MyComputer => "my-computer",
            Icon::Folder => "folder-closed",
            Icon::File => "file-generic",
            Icon::Text => "file-text",
            Icon::Application => "file-app",
            Icon::System => "file-system",
            Icon::Disk => "drive-hdd",
            Icon::OpticalDisc => "drive-cd",
            Icon::ControlPanel => "control-panel",
            Icon::MyDocuments => "my-documents",
            Icon::RecycleBin => "recycle-bin-empty",
            Icon::DateTime => "date-time",
            Icon::Sound => "sound-volume",
            Icon::DisplaySettings => "display-properties",
            Icon::Users => "users",
            Icon::NetworkPlaces => "network-places",
            Icon::EntireNetwork => "entire-network",
            Icon::Network => "network-connections",
            Icon::LocalConnection => "network-local",
            Icon::DisconnectedConnection => "network-disconnected",
            Icon::DeviceManager => "device-manager",
        }
    }

    /// 桌面条目 `Icon=` 名称对应的图标；不认识的名称用控制面板图标。
    pub fn from_name(name: Option<&str>) -> Icon {
        match name {
            Some("my-computer") => Icon::MyComputer,
            Some("date-time") => Icon::DateTime,
            Some("sound-volume") => Icon::Sound,
            Some("display-properties") => Icon::DisplaySettings,
            Some("users") => Icon::Users,
            Some("network-connections") => Icon::Network,
            Some("device-manager") => Icon::DeviceManager,
            _ => Icon::ControlPanel,
        }
    }
}

/// 详细资料列；数值即列标识，排序与表头事件共用。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum Field {
    Name = 1,
    /// 文件大小（KB，StrFormatKBSize）。
    Size,
    Kind,
    Modified,
    /// 磁盘总容量（StrFormatByteSize）。
    Capacity,
    Free,
    Comment,
    Original,
    Deleted,
    Status,
    Device,
    Owner,
}

impl Field {
    pub fn from_id(id: u64) -> Option<Self> {
        [
            Field::Name,
            Field::Size,
            Field::Kind,
            Field::Modified,
            Field::Capacity,
            Field::Free,
            Field::Comment,
            Field::Original,
            Field::Deleted,
            Field::Status,
            Field::Device,
            Field::Owner,
        ]
        .into_iter()
        .find(|field| *field as u64 == id)
    }

    /// 大小类列右对齐。
    pub fn right_aligned(self) -> bool {
        matches!(self, Field::Size | Field::Capacity | Field::Free)
    }

    /// 数值与日期列按数值比较，首次点击按降序排列（与 Windows 2000 一致）。
    pub fn numeric(self) -> bool {
        matches!(
            self,
            Field::Size | Field::Capacity | Field::Free | Field::Modified | Field::Deleted
        )
    }
}

/// 一列：字段、表头文字、默认宽度。
pub type ColumnSpec = (Field, &'static str, i32);

/// 条目在所在位置特有的详细资料。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Details {
    None,
    /// 磁盘的总容量与可用空间；statvfs 失败时为空。
    Drive(Option<(u64, u64)>),
    /// 控制面板项的备注（桌面条目 `Comment`）。
    Comment(String),
    /// 回收站条目的原位置（所在文件夹）与删除时刻。
    Trashed {
        original: PathBuf,
        deleted: Option<i64>,
    },
    /// 网络连接的状态与设备名。
    Connection {
        status: String,
        device: String,
    },
}

/// 一个可显示、可打开的条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub name: String,
    pub icon: Icon,
    /// “类型”列文本，如“文件夹”“文本文档”。
    pub kind: String,
    /// 普通文件的字节数；文件夹与虚拟条目没有大小。
    pub size: Option<u64>,
    /// 自 Unix epoch 起的秒数。
    pub modified: Option<i64>,
    /// 打开该条目时跳转的位置；`None` 表示不是文件夹（文件，由调用者决定如何处理）。
    pub target: Option<Location>,
    pub path: Option<PathBuf>,
    /// 控制面板项与网络连接的启动命令（程序与参数）；其他条目没有。
    pub launch: Option<Vec<String>>,
    pub details: Details,
}

impl Item {
    fn new(name: impl Into<String>, icon: Icon, kind: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            icon,
            kind: kind.into(),
            size: None,
            modified: None,
            target: None,
            path: None,
            launch: None,
            details: Details::None,
        }
    }

    /// 排序时的分组，任何排序列都先按它分组：驱动器、文件夹（含系统文件夹）、文件
    /// （Windows 2000“我的电脑”磁盘在前、控制面板在后，目录中文件夹在前）。
    pub fn group(&self) -> u8 {
        match (&self.details, &self.target) {
            (Details::Drive(_), _) => 0,
            (_, Some(_)) => 1,
            _ => 2,
        }
    }

    /// 详细资料中 `field` 列的文字。
    pub fn cell(&self, field: Field) -> String {
        match (field, &self.details) {
            (Field::Name, _) => self.name.clone(),
            (Field::Kind, _) => self.kind.clone(),
            (Field::Size, _) => self.size.map_or_else(String::new, size_text),
            (Field::Modified, _) => self.modified.map_or_else(String::new, date_text),
            (Field::Capacity, Details::Drive(Some((total, _)))) => byte_size_text(*total),
            (Field::Free, Details::Drive(Some((_, free)))) => byte_size_text(*free),
            (Field::Comment, Details::Comment(comment)) => comment.clone(),
            (Field::Original, Details::Trashed { original, .. }) => {
                original.to_string_lossy().into_owned()
            }
            (Field::Deleted, Details::Trashed { deleted, .. }) => {
                deleted.map_or_else(String::new, date_text)
            }
            (Field::Status, Details::Connection { status, .. }) => status.clone(),
            (Field::Device, Details::Connection { device, .. }) => device.clone(),
            (Field::Owner, Details::Connection { .. }) => "系统".into(),
            _ => String::new(),
        }
    }

    /// 按 `field` 列比较；数值列按数值，文字列按不区分大小写的文字。
    pub fn compare(&self, other: &Self, field: Field) -> Ordering {
        let number = |item: &Self| -> Option<i128> {
            match (field, &item.details) {
                (Field::Size, _) => item.size.map(i128::from),
                (Field::Modified, _) => item.modified.map(i128::from),
                (Field::Capacity, Details::Drive(capacity)) => {
                    capacity.map(|(total, _)| i128::from(total))
                }
                (Field::Free, Details::Drive(capacity)) => {
                    capacity.map(|(_, free)| i128::from(free))
                }
                (Field::Deleted, Details::Trashed { deleted, .. }) => deleted.map(i128::from),
                _ => None,
            }
        };
        if field.numeric() {
            number(self).cmp(&number(other))
        } else {
            self.cell(field)
                .to_lowercase()
                .cmp(&other.cell(field).to_lowercase())
        }
    }
}

impl Location {
    /// 解析命令行参数或地址栏文字：`::` 开头的虚拟文件夹名、虚拟文件夹的显示名，否则为文件系统路径。
    pub fn parse(argument: &str) -> Self {
        match argument {
            "::desktop" | "桌面" => Location::Desktop,
            "::my-computer" | "我的电脑" => Location::MyComputer,
            "::control-panel" | "控制面板" => Location::ControlPanel,
            "::network-places" | "网上邻居" => Location::NetworkPlaces,
            "::entire-network" | "整个网络" => Location::EntireNetwork,
            "::network-connections" | "网络和拨号连接" => Location::NetworkConnections,
            path => Location::Directory(PathBuf::from(path)),
        }
    }

    /// 地址栏文本。
    pub fn address(&self) -> String {
        match self {
            Location::Desktop => "桌面".into(),
            Location::MyComputer => "我的电脑".into(),
            Location::ControlPanel => "控制面板".into(),
            Location::NetworkPlaces => "网上邻居".into(),
            Location::EntireNetwork => "整个网络".into(),
            Location::NetworkConnections => "网络和拨号连接".into(),
            Location::Directory(path) => path.to_string_lossy().into_owned(),
        }
    }

    /// 窗口标题与信息面板标题：用户目录叫“我的文档”，回收站目录叫“回收站”，
    /// 其余目录取末级名称，根目录叫“本地磁盘”。
    pub fn title(&self) -> String {
        match self {
            Location::Directory(path) => well_known(path).map_or_else(
                || {
                    path.file_name()
                        .map_or_else(|| "本地磁盘".into(), |n| n.to_string_lossy().into_owned())
                },
                |(title, _)| title.into(),
            ),
            other => other.address(),
        }
    }

    /// 位置本身的图标（Web 视图、地址栏与区域格）。
    pub fn icon(&self) -> Icon {
        match self {
            Location::Desktop => Icon::Desktop,
            Location::MyComputer => Icon::MyComputer,
            Location::ControlPanel => Icon::ControlPanel,
            Location::NetworkPlaces => Icon::NetworkPlaces,
            Location::EntireNetwork => Icon::EntireNetwork,
            Location::NetworkConnections => Icon::Network,
            Location::Directory(path) => well_known(path).map_or_else(
                || {
                    if path.parent().is_none() {
                        Icon::Disk
                    } else {
                        Icon::Folder
                    }
                },
                |(_, icon)| icon,
            ),
        }
    }

    /// 是否为回收站根目录；只有根目录使用回收站的列与 Web 视图。
    pub fn is_recycle_bin(&self) -> bool {
        matches!(self, Location::Directory(path) if well_known(path).is_some_and(|(_, icon)| icon == Icon::RecycleBin))
    }

    /// “向上”的目标（Windows 2000 的命名空间层次）：我的电脑、网上邻居、我的文档、回收站到“桌面”；
    /// 根目录到“我的电脑”，其余目录到父目录；“控制面板”到“我的电脑”，“网络和拨号连接”到“控制面板”，
    /// “整个网络”到“网上邻居”。“桌面”是根。
    pub fn parent(&self) -> Option<Location> {
        match self {
            Location::Desktop => None,
            Location::MyComputer | Location::NetworkPlaces => Some(Location::Desktop),
            Location::ControlPanel => Some(Location::MyComputer),
            Location::NetworkConnections => Some(Location::ControlPanel),
            Location::EntireNetwork => Some(Location::NetworkPlaces),
            Location::Directory(path) if well_known(path).is_some() => Some(Location::Desktop),
            Location::Directory(path) => Some(
                path.parent()
                    .map_or(Location::MyComputer, |p| Location::Directory(p.into())),
            ),
        }
    }

    /// 详细资料的列（shell32 8976–8986、9386–9389，netshell 1000–1004）。
    pub fn columns(&self) -> &'static [ColumnSpec] {
        match self {
            Location::MyComputer => &[
                (Field::Name, "名称", 180),
                (Field::Kind, "类型", 110),
                (Field::Capacity, "大小", 90),
                (Field::Free, "可用空间", 90),
            ],
            Location::Desktop => &[
                (Field::Name, "名称", 200),
                (Field::Size, "大小", 100),
                (Field::Kind, "类型", 145),
                (Field::Modified, "修改时间", 160),
            ],
            Location::ControlPanel | Location::NetworkPlaces | Location::EntireNetwork => {
                &[(Field::Name, "名称", 180), (Field::Comment, "备注", 360)]
            }
            Location::NetworkConnections => &[
                (Field::Name, "名称", 140),
                (Field::Kind, "类型", 70),
                (Field::Status, "状态", 140),
                (Field::Device, "设备名", 160),
                (Field::Owner, "所有者", 70),
            ],
            location if location.is_recycle_bin() => &[
                (Field::Name, "名称", 160),
                (Field::Original, "原位置", 180),
                (Field::Deleted, "删除日期", 130),
                (Field::Kind, "类型", 110),
                (Field::Size, "大小", 70),
            ],
            Location::Directory(_) => &[
                (Field::Name, "名称", 200),
                (Field::Size, "大小", 100),
                (Field::Kind, "类型", 145),
                (Field::Modified, "修改时间", 160),
            ],
        }
    }

    /// 列出内容；目录不可读返回其 I/O 错误。第二个返回值表示被截断。
    ///
    /// `mounts` 是 `/proc/mounts` 的文本，只有“我的电脑”用它列出磁盘。
    pub fn list(&self, mounts: &str) -> io::Result<(Vec<Item>, bool)> {
        let mut truncated = false;
        let mut items = match self {
            Location::Desktop => {
                let home =
                    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
                let place = |name: &str, icon, kind: &str, target| Item {
                    target: Some(target),
                    ..Item::new(name, icon, kind)
                };
                let mut items = vec![
                    place(
                        "我的文档",
                        Icon::MyDocuments,
                        "系统文件夹",
                        Location::Directory(home.clone()),
                    ),
                    place(
                        "我的电脑",
                        Icon::MyComputer,
                        "系统文件夹",
                        Location::MyComputer,
                    ),
                    place(
                        "网上邻居",
                        Icon::NetworkPlaces,
                        "系统文件夹",
                        Location::NetworkPlaces,
                    ),
                    place(
                        "回收站",
                        Icon::RecycleBin,
                        "系统文件夹",
                        Location::Directory(fileops::trash_files(&home)),
                    ),
                ];
                // 桌面文件夹缺失时只显示系统图标，与桌面程序的行为一致。
                let directory = home.join("Desktop");
                let mut files: Vec<Item> = fs::read_dir(&directory)
                    .into_iter()
                    .flatten()
                    .filter_map(Result::ok)
                    .take(MAX_ENTRIES)
                    .map(|entry| describe(&entry.path(), entry.file_name().to_string_lossy()))
                    .collect();
                files.sort_by(|a, b| {
                    b.target
                        .is_some()
                        .cmp(&a.target.is_some())
                        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                });
                items.extend(files);
                return Ok((items, false));
            }
            Location::MyComputer => {
                let mut items = drives(mounts);
                let mut panel = Item::new("控制面板", Icon::ControlPanel, "系统文件夹");
                panel.target = Some(Location::ControlPanel);
                items.push(panel);
                return Ok((items, false));
            }
            Location::ControlPanel => return Ok((applets(), false)),
            Location::NetworkPlaces => {
                let mut entire = Item::new("整个网络", Icon::EntireNetwork, "");
                entire.target = Some(Location::EntireNetwork);
                return Ok((vec![entire], false));
            }
            Location::EntireNetwork => return Ok((Vec::new(), false)),
            Location::NetworkConnections => {
                return Ok((connections(Path::new("/sys/class/net")), false));
            }
            Location::Directory(path) => {
                let trash = self.is_recycle_bin();
                let home = std::env::var_os("HOME").map(PathBuf::from);
                let mut items = Vec::new();
                for entry in fs::read_dir(path)? {
                    let Ok(entry) = entry else { continue };
                    if items.len() == MAX_ENTRIES {
                        truncated = true;
                        break;
                    }
                    let mut item = describe(&entry.path(), entry.file_name().to_string_lossy());
                    if trash
                        && let Some(home) = &home
                        && let Ok(info) = fileops::trash_info_of(home, &entry.path())
                    {
                        // 回收站显示原名称；回收站内的文件夹不可进入（Windows 2000 打开的是属性）。
                        if let Some(name) = info.original.file_name() {
                            item.name = name.to_string_lossy().into_owned();
                        }
                        item.target = None;
                        item.details = Details::Trashed {
                            original: info.original.parent().unwrap_or(Path::new("/")).into(),
                            deleted: info.deleted,
                        };
                    }
                    items.push(item);
                }
                items
            }
        };
        items.sort_by(|a, b| {
            b.target
                .is_some()
                .cmp(&a.target.is_some())
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok((items, truncated))
    }
}

/// 网络和拨号连接：每个以太网类接口一项，按接口序号命名“本地连接”“本地连接 2”……（netshell 16022）。
/// 状态取管理状态与载波（netshell 1047/1048/1050），设备名取驱动名，打开即“本地连接 状态”。
fn connections(root: &Path) -> Vec<Item> {
    let read = |name: &str, file: &str| {
        fs::read_to_string(root.join(name).join(file))
            .map(|text| text.trim().to_owned())
            .unwrap_or_default()
    };
    let mut interfaces: Vec<(u64, String)> = fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().to_string_lossy().into_owned();
            // ARPHRD_ETHER；回环（772）等其他链路类型不是“本地连接”。
            (read(&name, "type") == "1")
                .then(|| (read(&name, "ifindex").parse().unwrap_or(u64::MAX), name))
        })
        .collect();
    interfaces.sort();
    interfaces
        .into_iter()
        .enumerate()
        .map(|(index, (_, interface))| {
            let flags = u32::from_str_radix(read(&interface, "flags").trim_start_matches("0x"), 16)
                .unwrap_or(0);
            let (status, icon) = if flags & 1 == 0 {
                ("禁用", Icon::DisconnectedConnection)
            } else if read(&interface, "carrier") != "1" {
                ("网络电缆被拔出", Icon::DisconnectedConnection)
            } else {
                ("已启用", Icon::LocalConnection)
            };
            let device = fs::read_link(root.join(&interface).join("device/driver"))
                .ok()
                .and_then(|driver| {
                    driver
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .unwrap_or_else(|| interface.clone());
            let name = if index == 0 {
                "本地连接".to_owned()
            } else {
                format!("本地连接 {}", index + 1)
            };
            Item {
                launch: Some(vec![
                    "/bin/control-panel".into(),
                    "network".into(),
                    interface,
                ]),
                details: Details::Connection {
                    status: status.into(),
                    device,
                },
                ..Item::new(name, icon, "LAN")
            }
        })
        .collect()
}

/// 控制面板的内容：已安装的、分类为 `Settings` 的桌面条目；没有安装任何设置程序时为空。
/// 启动资源管理器某个 `::` 位置的条目是外壳文件夹（如“网络和拨号连接”），在当前窗口中打开。
fn applets() -> Vec<Item> {
    desktop_entry::load(Path::new("/usr/share/applications"))
        .into_iter()
        .filter(|entry| entry.in_category("Settings"))
        .map(|entry| {
            let arguments = entry.arguments().to_vec();
            let target = match arguments.as_slice() {
                [program, location]
                    if Path::new(program).file_name() == Some("file-explorer".as_ref())
                        && location.starts_with("::") =>
                {
                    Some(Location::parse(location))
                }
                _ => None,
            };
            Item {
                launch: target.is_none().then_some(arguments),
                target,
                details: Details::Comment(entry.comment.clone().unwrap_or_default()),
                ..Item::new(
                    entry.name.clone(),
                    Icon::from_name(entry.icon.as_deref()),
                    "控制面板项",
                )
            }
        })
        .collect()
}

/// 桌面图标对应的用户目录：`$HOME` 是“我的文档”，其回收站目录是“回收站”。
/// 它们仍是普通目录，只是名称与图标沿用 Windows 2000 的叫法。
fn well_known(path: &Path) -> Option<(&'static str, Icon)> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let trash = fileops::trash_files(&home);
    if path == home {
        Some(("我的文档", Icon::MyDocuments))
    } else if path == trash || fs::canonicalize(&trash).is_ok_and(|trash| path == trash) {
        Some(("回收站", Icon::RecycleBin))
    } else {
        None
    }
}

/// 从 `/proc/mounts` 文本挑出真实存储：只认磁盘类文件系统，同一挂载点只出现一次。
/// 类型名取 shell32 9397（本地磁盘）/ 9399（光盘），容量由 statvfs 读取。
fn drives(mounts: &str) -> Vec<Item> {
    const DISK: &[&str] = &[
        "ext2", "ext3", "ext4", "vfat", "msdos", "exfat", "xfs", "btrfs", "erofs", "squashfs",
        "9p", "virtiofs",
    ];
    const OPTICAL: &[&str] = &["iso9660", "udf"];
    let mut seen = Vec::new();
    let mut items = Vec::new();
    for line in mounts.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        let [_, mount, kind, ..] = fields[..] else {
            continue;
        };
        let optical = OPTICAL.contains(&kind);
        if !(optical || DISK.contains(&kind)) || seen.contains(&mount) {
            continue;
        }
        seen.push(mount);
        let (type_name, icon) = if optical {
            ("光盘", Icon::OpticalDisc)
        } else {
            ("本地磁盘", Icon::Disk)
        };
        let capacity = linux_uapi::filesystem::capacity(Path::new(mount))
            .ok()
            .map(|capacity| (capacity.total, capacity.free));
        items.push(Item {
            target: Some(Location::Directory(mount.into())),
            path: Some(mount.into()),
            details: Details::Drive(capacity),
            ..Item::new(format!("{type_name} ({mount})"), icon, type_name)
        });
    }
    items
}

/// 描述一个文件系统条目；元数据读取失败时按普通文件处理，不让单个坏条目挡住整个目录。
fn describe(path: &Path, name: impl Into<String>) -> Item {
    let metadata = fs::metadata(path).ok();
    let directory = metadata.as_ref().is_some_and(fs::Metadata::is_dir);
    let executable = metadata
        .as_ref()
        .is_some_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let (icon, kind) = if directory {
        (Icon::Folder, "文件夹".to_owned())
    } else if executable && extension.is_empty() {
        (Icon::Application, "应用程序".to_owned())
    } else {
        match extension.as_str() {
            "txt" | "log" | "md" | "conf" | "cfg" | "ini" | "rs" | "c" | "h" | "py" | "sh"
            | "toml" => (Icon::Text, "文本文档".to_owned()),
            "so" | "ko" | "a" | "o" | "dll" | "sys" => {
                (Icon::System, format!("{} 文件", extension.to_uppercase()))
            }
            "" => (Icon::File, "文件".to_owned()),
            other => (Icon::File, format!("{} 文件", other.to_uppercase())),
        }
    };
    Item {
        size: (!directory).then(|| metadata.as_ref().map_or(0, fs::Metadata::len)),
        modified: metadata
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64),
        target: directory.then(|| Location::Directory(path.into())),
        path: Some(path.into()),
        ..Item::new(name, icon, kind)
    }
}

/// 千分位分组（简体中文区域设置的数字格式）。
fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut text = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            text.push(',');
        }
        text.push(digit);
    }
    text
}

/// shlwapi `StrFormatByteSize`：小于 1024 显示“N 字节”，否则取 KB/MB/GB/TB/PB/EB 中使数值小于 1000 的单位，
/// 保留三位有效数字并截断（不四舍五入），单位文字取 shlwapi 259–265。
pub fn byte_size_text(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} 字节");
    }
    const UNITS: [&str; 6] = ["KB", "MB", "GB", "TB", "PB", "EB"];
    let mut scale = 1024f64;
    for (index, unit) in UNITS.iter().enumerate() {
        let value = bytes as f64 / scale;
        if value < 1000.0 || index == UNITS.len() - 1 {
            let decimals = if value < 10.0 {
                2
            } else if value < 100.0 {
                1
            } else {
                0
            };
            let factor = 10f64.powi(decimals);
            let truncated = (value * factor + 1e-9).floor() / factor;
            return format!("{truncated:.prec$} {unit}", prec = decimals as usize);
        }
        scale *= 1024.0;
    }
    unreachable!("the last unit always formats")
}

/// 详细信息的“大小”列（shlwapi `StrFormatKBSize`）：以 KB 为单位向上取整并千分位分组，空文件显示 0 KB。
pub fn size_text(bytes: u64) -> String {
    format!("{} KB", grouped(bytes.div_ceil(1024)))
}

/// 详细信息的"修改时间"列：`年-月-日 时:分`，月日不补零（简体中文 Windows 2000 的短日期风格）；
/// 按系统本地时区显示（`/etc/localtime`）。
pub fn date_text(seconds: i64) -> String {
    let seconds = calendar::zone::to_local(seconds);
    let date = calendar::civil_from_days(seconds.div_euclid(86400));
    let rest = seconds.rem_euclid(86400);
    format!(
        "{}-{}-{} {}:{:02}",
        date.year,
        date.month,
        date.day,
        rest / 3600,
        rest % 3600 / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_follow_the_proleptic_gregorian_calendar() {
        assert_eq!(date_text(0), "1970-1-1 0:00");
        assert_eq!(
            date_text(951_782_400 + 3600 * 13 + 60 * 5),
            "2000-2-29 13:05"
        );
        assert_eq!(date_text(-86_400), "1969-12-31 0:00");
        assert_eq!(date_text(4_102_444_800 - 1), "2099-12-31 23:59");
    }

    #[test]
    fn sizes_round_up_to_kilobytes() {
        assert_eq!(size_text(0), "0 KB");
        assert_eq!(size_text(1), "1 KB");
        assert_eq!(size_text(1025), "2 KB");
        assert_eq!(size_text(1_300_000), "1,270 KB");
    }

    #[test]
    fn byte_sizes_keep_three_significant_digits_and_truncate() {
        assert_eq!(byte_size_text(1000), "1000 字节");
        assert_eq!(byte_size_text(1500), "1.46 KB");
        assert_eq!(byte_size_text(1023 * 1024), "0.99 MB");
        assert_eq!(byte_size_text(4_294_967_296), "4.00 GB");
        assert_eq!(byte_size_text(150 * 1024 * 1024), "150 MB");
    }

    #[test]
    fn only_storage_mounts_become_drives_and_each_mount_point_once() {
        let mounts = "/dev/vda / ext4 rw 0 0\nproc /proc proc rw 0 0\n\
                      tmpfs /run tmpfs rw 0 0\n/dev/sr0 /mnt/cd iso9660 ro 0 0\n\
                      /dev/vda / ext4 rw 0 0\n";
        let names: Vec<_> = drives(mounts).into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["本地磁盘 (/)", "光盘 (/mnt/cd)"]);
    }

    #[test]
    fn the_home_directory_is_my_documents() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let home = Location::Directory(PathBuf::from(&home));
        assert_eq!(home.title(), "我的文档");
        assert_eq!(
            well_known(Path::new(&home.address())).map(|(_, icon)| icon),
            Some(Icon::MyDocuments)
        );
        let trash =
            Location::Directory(PathBuf::from(home.address()).join(".local/share/Trash/files"));
        assert_eq!(trash.title(), "回收站");
    }

    #[test]
    fn parent_walks_up_to_my_computer() {
        let at = |p: &str| Location::Directory(p.into());
        assert_eq!(at("/usr/bin").parent(), Some(at("/usr")));
        assert_eq!(at("/usr").parent(), Some(at("/")));
        assert_eq!(at("/").parent(), Some(Location::MyComputer));
        assert_eq!(Location::ControlPanel.parent(), Some(Location::MyComputer));
        assert_eq!(Location::MyComputer.parent(), Some(Location::Desktop));
        assert_eq!(Location::Desktop.parent(), None);
        assert_eq!(Location::parse("::my-computer"), Location::MyComputer);
        assert_eq!(at("/").title(), "本地磁盘");
    }
}
