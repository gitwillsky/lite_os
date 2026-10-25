//! 属性数据在后台读取/统计，界面使用标准标签、复选框和共享属性表应用阶段。
use crate::{Apply, Page, child, image, label, pair};
use liteos_ui::{Draw, Event, Layout, PropertySheet, Rect, Widget, WidgetKind as Kind, state};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};
const WALK_LIMIT: u64 = 50_000;

/// 磁盘的已用/可用/总容量（字节）；读取失败时属性页显示不可用。
fn capacity(mount: &Path) -> Option<(u64, u64, u64)> {
    let stat = linux_uapi::filesystem::capacity(mount).ok()?;
    Some((stat.used, stat.free, stat.total))
}

/// `1,234,567` 样式的千分位。
fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// Windows 2000 的容量文本：`1.23 KB (1,264 字节)`，不足 1 KB 只写字节数。
fn size_text(bytes: u64) -> String {
    const UNITS: [(&str, u64); 3] = [("GB", 1 << 30), ("MB", 1 << 20), ("KB", 1 << 10)];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            return format!(
                "{:.2} {unit} ({} 字节)",
                bytes as f64 / scale as f64,
                grouped(bytes)
            );
        }
    }
    format!("{} 字节", grouped(bytes))
}

/// `2026年10月25日 19:30:12`，按系统本地时区显示。
fn date_text(seconds: i64) -> String {
    let seconds = calendar::zone::to_local(seconds);
    let date = calendar::civil_from_days(seconds.div_euclid(86400));
    let rest = seconds.rem_euclid(86400);
    format!(
        "{}年{}月{}日 {}:{:02}:{:02}",
        date.year,
        date.month,
        date.day,
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

fn seconds(time: std::io::Result<std::time::SystemTime>) -> Option<i64> {
    time.ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

/// 递归统计目录：`(字节数, 占用字节数, 文件数, 文件夹数, 是否被上限截断)`。
fn walk(root: &Path) -> (u64, u64, u64, u64, bool) {
    let (mut bytes, mut on_disk, mut files, mut folders) = (0, 0, 0, 0);
    let mut pending = vec![root.to_path_buf()];
    let mut visited = 0u64;
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > WALK_LIMIT {
                return (bytes, on_disk, files, folders, true);
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            on_disk += metadata.blocks() * 512;
            if metadata.is_dir() {
                folders += 1;
                pending.push(entry.path());
            } else {
                files += 1;
                bytes += metadata.len();
            }
        }
    }
    (bytes, on_disk, files, folders, false)
}

struct Entry {
    kind: String,
    location: String,
    size: u64,
    on_disk: u64,
    /// 文件夹的 `(文件数, 文件夹数, 被截断)`。
    contents: Option<(u64, u64, bool)>,
    created: Option<i64>,
    modified: Option<i64>,
    accessed: Option<i64>,
    hidden: bool,
}

struct Drive {
    kind: String,
    filesystem: String,
    /// `(已用, 可用, 总容量)`。
    capacity: Option<(u64, u64, u64)>,
}

enum Body {
    Entry(Entry),
    Drive(Drive),
}

pub struct Data {
    path: PathBuf,
    name: String,
    icon: String,
    body: Body,
    readonly: bool,
}

/// 挂载点对应的文件系统类型，取自 `/proc/mounts`。
fn filesystem_of(mount: &Path) -> String {
    fs::read_to_string("/proc/mounts")
        .ok()
        .and_then(|mounts| {
            mounts.lines().find_map(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                (fields.get(1) == Some(&mount.to_string_lossy().as_ref()))
                    .then(|| fields.get(2).map(|kind| (*kind).to_owned()))
                    .flatten()
            })
        })
        .unwrap_or_default()
}

impl Data {
    pub fn read(mut args: impl Iterator<Item = std::ffi::OsString>) -> Result<Self, String> {
        let path = PathBuf::from(args.next().ok_or("missing path")?);
        let icon_stem = args
            .next()
            .ok_or("missing icon")?
            .into_string()
            .map_err(|_| "invalid icon")?;
        let kind = args
            .next()
            .ok_or("missing type")?
            .to_string_lossy()
            .into_owned();
        let drive = args.next().is_some_and(|argument| argument == "drive");
        let metadata =
            fs::metadata(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        let readonly = metadata.permissions().mode() & 0o200 == 0;
        let (name, body) = if drive {
            (
                kind.clone(),
                Body::Drive(Drive {
                    kind,
                    filesystem: filesystem_of(&path),
                    capacity: capacity(&path),
                }),
            )
        } else {
            let name = path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
            let (size, on_disk, contents) = if metadata.is_dir() {
                let (bytes, on_disk, files, folders, cut) = walk(&path);
                (bytes, on_disk, Some((files, folders, cut)))
            } else {
                (metadata.len(), metadata.blocks() * 512, None)
            };
            let body = Body::Entry(Entry {
                kind,
                location: path
                    .parent()
                    .map_or_else(String::new, |p| p.display().to_string()),
                size,
                on_disk,
                contents,
                created: seconds(metadata.created()).or(Some(metadata.ctime())),
                modified: seconds(metadata.modified()),
                accessed: seconds(metadata.accessed()),
                hidden: name.starts_with('.'),
            });
            (name, body)
        };
        Ok(Self {
            path,
            name,
            icon: icon_stem,
            body,
            readonly,
        })
    }
}

pub struct FileProperties {
    _controls: Vec<Widget>,
    path: PathBuf,
    readonly: Option<Widget>,
    saved: bool,
    pie: Option<(Widget, u64, u64)>,
}
impl FileProperties {
    pub fn build(sheet: &PropertySheet, data: Data) -> liteos_ui::Result<Self> {
        sheet.window().set_title(&format!("{} 属性", data.name))?;
        let page = sheet.page("常规")?;
        let mut controls = vec![image(&page, &data.icon)?, label(&page, &data.name)?];
        let mut readonly = None;
        let mut pie = None;
        let mut field = |title: &str, text: &str| -> liteos_ui::Result<()> {
            let (a, b) = pair(&page, title, text)?;
            controls.extend([a, b]);
            Ok(())
        };
        match data.body {
            Body::Entry(entry) => {
                field("类型：", &entry.kind)?;
                field("位置：", &entry.location)?;
                let prefix = if entry.contents.is_some_and(|(_, _, cut)| cut) {
                    "至少 "
                } else {
                    ""
                };
                field("大小：", &format!("{prefix}{}", size_text(entry.size)))?;
                field(
                    "占用空间：",
                    &format!("{prefix}{}", size_text(entry.on_disk)),
                )?;
                if let Some((files, folders, _)) = entry.contents {
                    field("包含：", &format!("{files} 个文件，{folders} 个文件夹"))?;
                }
                for (title, time) in [
                    ("创建时间：", entry.created),
                    ("修改时间：", entry.modified),
                    ("访问时间：", entry.accessed),
                ] {
                    field(title, &time.map(date_text).unwrap_or_default())?;
                }
                let option = child(&page, Kind::CheckBox, "只读(R)", Layout::default())?;
                option.set_state(if data.readonly { state::CHECKED } else { 0 })?;
                readonly = Some(option);
                let hidden = child(&page, Kind::CheckBox, "隐藏(H)", Layout::default())?;
                hidden
                    .set_state(state::DISABLED | if entry.hidden { state::CHECKED } else { 0 })?;
                controls.push(hidden);
            }
            Body::Drive(drive) => {
                field("类型：", &drive.kind)?;
                field("文件系统：", &drive.filesystem)?;
                if let Some((used, free, total)) = drive.capacity {
                    field("已用空间：", &size_text(used))?;
                    field("可用空间：", &size_text(free))?;
                    field("容量：", &size_text(total))?;
                    let canvas = child(
                        &page,
                        Kind::Canvas,
                        "",
                        Layout {
                            height: 110,
                            ..Default::default()
                        },
                    )?;
                    pie = Some((canvas, used, total));
                } else {
                    field("容量：", "无法读取磁盘容量")?;
                }
                controls.push(label(&page, &format!("驱动器 {}", data.path.display()))?);
            }
        }
        controls.push(page);
        let view = Self {
            _controls: controls,
            path: data.path,
            readonly,
            saved: data.readonly,
            pie,
        };
        view.preview()?;
        Ok(view)
    }
    fn value(&self) -> liteos_ui::Result<bool> {
        self.readonly.as_ref().map_or(Ok(self.saved), |control| {
            control.state().map(|value| value & state::CHECKED != 0)
        })
    }
    fn preview(&self) -> liteos_ui::Result<()> {
        let Some((canvas, used, total)) = &self.pie else {
            return Ok(());
        };
        let area = canvas.geometry()?;
        if area.width <= 0 || area.height <= 0 {
            return Ok(());
        }
        let share = if *total == 0 {
            0.0
        } else {
            *used as f64 / *total as f64
        };
        let rx = area.width.min(150) / 2;
        let ry = (area.height - 14).min(70) / 2;
        let center = (area.width / 2, area.height / 2);
        let mut commands = Vec::new();
        for y in -ry..=ry {
            let span = (f64::from(rx)
                * (1.0 - f64::from(y * y) / f64::from((ry * ry).max(1)))
                    .max(0.0)
                    .sqrt()) as i32;
            for x in -span..=span {
                let angle = (f64::from(y) / f64::from(ry.max(1)))
                    .atan2(f64::from(x) / f64::from(rx.max(1)))
                    .rem_euclid(std::f64::consts::TAU);
                let color = if angle < std::f64::consts::TAU * share {
                    0x0000ff
                } else {
                    0xff00ff
                };
                commands.push(Draw {
                    kind: 1,
                    rect: Rect::new(center.0 + x, center.1 + y, 1, 1),
                    color,
                    bold: false,
                    text: "",
                });
            }
        }
        canvas.set_drawing(&commands)
    }
}
impl Page for FileProperties {
    fn event(&mut self, _event: &Event) -> liteos_ui::Result<Option<Vec<String>>> {
        self.preview()?;
        Ok(None)
    }
    fn dirty(&self) -> liteos_ui::Result<bool> {
        Ok(self.value()? != self.saved)
    }
    fn apply(&self) -> liteos_ui::Result<Apply> {
        let readonly = self.value()?;
        let path = self.path.clone();
        Ok(Box::new(move || {
            let mut permissions = fs::metadata(&path)
                .map_err(|error| error.to_string())?
                .permissions();
            let mode = permissions.mode();
            permissions.set_mode(if readonly {
                mode & !0o222
            } else {
                mode | 0o200
            });
            fs::set_permissions(&path, permissions).map_err(|error| error.to_string())
        }))
    }
    fn committed(&mut self) -> liteos_ui::Result<()> {
        self.saved = self.value()?;
        Ok(())
    }
}
