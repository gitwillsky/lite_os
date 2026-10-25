//! 资源管理器的位置与条目：真实目录加上"我的电脑""控制面板"两个虚拟文件夹。
use std::{
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
    MyComputer,
    ControlPanel,
    Directory(PathBuf),
}

/// 条目图标；与 `/usr/share/liteos/ui/<name>-{16,32}.bmp` 一一对应。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
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
}

impl Icon {
    pub fn file_stem(self) -> &'static str {
        match self {
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
            _ => Icon::ControlPanel,
        }
    }
}

/// 一个可显示、可打开的条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub name: String,
    pub icon: Icon,
    /// "类型"列文本，如"文件夹""文本文档"。
    pub kind: String,
    /// 普通文件的字节数；文件夹与虚拟条目没有大小。
    pub size: Option<u64>,
    /// 自 Unix epoch 起的秒数。
    pub modified: Option<i64>,
    /// 打开该条目时跳转的位置；`None` 表示不是文件夹（文件，由调用者决定如何处理）。
    pub target: Option<Location>,
    pub path: Option<PathBuf>,
    /// 控制面板项的启动命令（程序与参数）；其他条目没有。
    pub launch: Option<Vec<String>>,
}

impl Location {
    /// 解析命令行参数：`::my-computer`、`::control-panel` 或文件系统路径。
    pub fn parse(argument: &str) -> Self {
        match argument {
            "::my-computer" => Location::MyComputer,
            "::control-panel" => Location::ControlPanel,
            path => Location::Directory(PathBuf::from(path)),
        }
    }

    /// 地址栏文本。
    pub fn address(&self) -> String {
        match self {
            Location::MyComputer => "我的电脑".into(),
            Location::ControlPanel => "控制面板".into(),
            Location::Directory(path) => path.to_string_lossy().into_owned(),
        }
    }

    /// 窗口标题与信息面板标题：用户目录叫"我的文档"，回收站目录叫"回收站"，
    /// 其余目录取末级名称，根目录叫"本地磁盘"。
    pub fn title(&self) -> String {
        match self {
            Location::Directory(path) if well_known(path).is_some() => {
                well_known(path).map_or_else(String::new, |(title, _)| title.into())
            }
            Location::Directory(path) => path
                .file_name()
                .map_or_else(|| "本地磁盘".into(), |n| n.to_string_lossy().into_owned()),
            other => other.address(),
        }
    }

    pub fn icon(&self) -> Icon {
        match self {
            Location::MyComputer => Icon::MyComputer,
            Location::ControlPanel => Icon::ControlPanel,
            Location::Directory(path) if path == Path::new("/") => Icon::Disk,
            Location::Directory(path) if let Some((_, icon)) = well_known(path) => icon,
            Location::Directory(_) => Icon::Folder,
        }
    }

    /// "向上"的目标：目录到其父目录，根目录到"我的电脑"，"控制面板"到"我的电脑"。
    pub fn parent(&self) -> Option<Location> {
        match self {
            Location::MyComputer => None,
            Location::ControlPanel => Some(Location::MyComputer),
            Location::Directory(path) => Some(
                path.parent()
                    .map_or(Location::MyComputer, |p| Location::Directory(p.into())),
            ),
        }
    }

    /// 列出内容；目录不可读返回其 I/O 错误。第二个返回值表示被截断。
    ///
    /// `mounts` 是 `/proc/mounts` 的文本，只有"我的电脑"用它列出磁盘。
    pub fn list(&self, mounts: &str) -> io::Result<(Vec<Item>, bool)> {
        let mut truncated = false;
        let mut items = match self {
            Location::MyComputer => {
                let mut items = drives(mounts);
                items.push(virtual_folder(
                    "控制面板",
                    Icon::ControlPanel,
                    "系统文件夹",
                    Location::ControlPanel,
                ));
                return Ok((items, false));
            }
            Location::ControlPanel => return Ok((applets(), false)),
            Location::Directory(path) => {
                let mut items = Vec::new();
                for entry in fs::read_dir(path)? {
                    let Ok(entry) = entry else { continue };
                    if items.len() == MAX_ENTRIES {
                        truncated = true;
                        break;
                    }
                    items.push(describe(&entry.path(), entry.file_name().to_string_lossy()));
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

/// 控制面板的内容：已安装的、分类为 `Settings` 的桌面条目；没有安装任何设置程序时为空。
fn applets() -> Vec<Item> {
    desktop_entry::load(Path::new("/usr/share/applications"))
        .into_iter()
        .filter(|entry| entry.in_category("Settings"))
        .map(|entry| Item {
            icon: Icon::from_name(entry.icon.as_deref()),
            launch: Some(entry.arguments().to_vec()),
            name: entry.name,
            kind: "控制面板项".into(),
            size: None,
            modified: None,
            target: None,
            path: None,
        })
        .collect()
}

/// 桌面图标对应的用户目录：`$HOME` 是"我的文档"，其回收站目录是"回收站"。
/// 它们仍是普通目录，只是名称与图标沿用 Windows 2000 的叫法。
fn well_known(path: &Path) -> Option<(&'static str, Icon)> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    if path == home {
        Some(("我的文档", Icon::MyDocuments))
    } else if path == home.join(".local/share/Trash/files") {
        Some(("回收站", Icon::RecycleBin))
    } else {
        None
    }
}

fn virtual_folder(name: &str, icon: Icon, kind: &str, target: Location) -> Item {
    Item {
        name: name.into(),
        icon,
        kind: kind.into(),
        size: None,
        modified: None,
        target: Some(target),
        path: None,
        launch: None,
    }
}

/// 从 `/proc/mounts` 文本挑出真实存储：只认磁盘类文件系统，同一挂载点只出现一次。
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
        let (label, icon, type_name) = if optical {
            ("CD 驱动器", Icon::OpticalDisc, "CD 驱动器")
        } else {
            ("本地磁盘", Icon::Disk, "本地磁盘")
        };
        items.push(Item {
            name: format!("{label} ({mount})"),
            icon,
            kind: type_name.into(),
            size: None,
            modified: None,
            target: Some(Location::Directory(mount.into())),
            path: Some(mount.into()),
            launch: None,
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
        name: name.into(),
        icon,
        kind,
        size: (!directory).then(|| metadata.as_ref().map_or(0, fs::Metadata::len)),
        modified: metadata
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64),
        target: directory.then(|| Location::Directory(path.into())),
        path: Some(path.into()),
        launch: None,
    }
}

/// 详细信息的"大小"列：Windows 2000 以 KB 为单位向上取整，空文件显示 0 KB。
pub fn size_text(bytes: u64) -> String {
    format!("{} KB", bytes.div_ceil(1024))
}

/// 详细信息的"修改时间"列：`年-月-日 时:分`，月日不补零（简体中文 Windows 2000 的短日期风格）；
/// 系统没有时区数据库，所以按 UTC 显示。
pub fn date_text(seconds: i64) -> String {
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
    }

    #[test]
    fn only_storage_mounts_become_drives_and_each_mount_point_once() {
        let mounts = "/dev/vda / ext4 rw 0 0\nproc /proc proc rw 0 0\n\
                      tmpfs /run tmpfs rw 0 0\n/dev/sr0 /mnt/cd iso9660 ro 0 0\n\
                      /dev/vda / ext4 rw 0 0\n";
        let names: Vec<_> = drives(mounts).into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["本地磁盘 (/)", "CD 驱动器 (/mnt/cd)"]);
    }

    #[test]
    fn the_home_directory_is_my_documents() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let home = Location::Directory(PathBuf::from(&home));
        assert_eq!(home.title(), "我的文档");
        assert_eq!(home.icon(), Icon::MyDocuments);
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
        assert_eq!(Location::MyComputer.parent(), None);
        assert_eq!(Location::parse("::my-computer"), Location::MyComputer);
        assert_eq!(at("/").title(), "本地磁盘");
    }
}
