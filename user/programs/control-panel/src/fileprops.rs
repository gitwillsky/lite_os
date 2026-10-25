//! 文件、文件夹与磁盘的"属性"：对应 Windows 2000 shell32 属性表的"常规"页。
//!
//! 由资源管理器以 `control-panel properties <路径> <图标名> <类型> [drive]` 启动，是独立的顶层窗口，
//! 所以可以拖动、用标题栏按钮关闭，也不会被资源管理器的重绘覆盖。"只读"是唯一可写的属性（chmod）；
//! "隐藏"在 Linux 上由文件名的前导点决定，改它等于重命名，所以只显示、不可改。
use crate::{Icons, Page};
use canvas::Rect;
use classic_theme::{Edge, Rgb, SystemColor as C};
use classic_ui::{Bitmap, Ink, Painter};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

/// 目录统计最多访问的条目数；超过时标注为"至少"，避免在 `/` 这样的大目录上阻塞窗口。
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

pub struct FileProperties {
    path: PathBuf,
    name: String,
    icon: Option<Bitmap>,
    body: Body,
    readonly: bool,
    readonly_at_open: bool,
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

impl FileProperties {
    /// 从命令行参数建立：`<路径> <图标名> <类型> [drive]`。
    pub fn from_args(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let path = PathBuf::from(args.next().ok_or("missing path")?);
        let icon_stem = args.next().ok_or("missing icon")?;
        let kind = args.next().ok_or("missing type")?;
        let drive = args.next().as_deref() == Some("drive");
        let icon = Bitmap::load(Path::new(&format!(
            "/usr/share/liteos/ui/{icon_stem}-32.bmp"
        )))
        .ok();
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
            icon,
            body,
            readonly,
            readonly_at_open: readonly,
        })
    }

    fn checkbox_rect(page: Rect) -> Rect {
        Rect::new(page.x + 80, page.y + 292, 13, 13)
    }
}

/// 在 `y` 处画一条凹陷的分隔线（Windows 2000 属性页的分组线）。
fn separator(p: &mut Painter<'_>, page: Rect, y: i32) {
    p.fill(
        Rect::new(page.x + 12, y, page.width - 24, 1),
        C::ButtonShadow,
    );
    p.fill(
        Rect::new(page.x + 12, y + 1, page.width - 24, 1),
        C::ButtonHilight,
    );
}

/// 标签与值同一行：标签靠左，值在固定列。
fn row(p: &mut Painter<'_>, page: Rect, y: i32, label: &str, value: &str) {
    let ink = Ink::Color(C::ButtonText);
    p.text_fit(page.x + 14, y, 78, label, ink, false);
    p.text_fit(page.x + 96, y, page.width - 108, value, ink, false);
}

/// 画一行并把 `y` 下移一个行距。
fn line(p: &mut Painter<'_>, page: Rect, y: &mut i32, step: i32, label: &str, value: &str) {
    row(p, page, *y, label, value);
    *y += step;
}

/// 图标与名称框：左侧 32×32 图标，右侧凹陷的只读文本框。
fn heading(p: &mut Painter<'_>, page: Rect, icon: Option<&Bitmap>, name: &str) {
    if let Some(icon) = icon {
        icon.paint(p, page.x + 14, page.y + 14);
    }
    let field = Rect::new(page.x + 68, page.y + 20, page.width - 84, 21);
    let inner = p.edge(field, Edge::Sunken);
    p.fill(inner, C::Window);
    p.clipped(inner, |p| {
        p.text(
            inner.x + 3,
            inner.y + 2,
            name,
            Ink::Color(C::WindowText),
            false,
        );
    });
}

const USED: Rgb = Rgb {
    red: 0,
    green: 0,
    blue: 255,
};
const USED_SIDE: Rgb = Rgb {
    red: 0,
    green: 0,
    blue: 128,
};
const FREE: Rgb = Rgb {
    red: 255,
    green: 0,
    blue: 255,
};
const FREE_SIDE: Rgb = Rgb {
    red: 128,
    green: 0,
    blue: 128,
};

/// 画带厚度的容量饼图：已用空间蓝色、可用空间洋红，与 Windows 2000 一致。
fn pie(p: &mut Painter<'_>, center: (i32, i32), used_share: f64) {
    const RX: f64 = 56.0;
    const RY: f64 = 28.0;
    const DEPTH: i32 = 10;
    // 已用扇区从 3 点钟方向起逆时针扫过 `used_share` 圈。
    let colour = |x: i32, y: i32, side: bool| {
        let (dx, dy) = (f64::from(x - center.0) / RX, f64::from(y - center.1) / RY);
        let turn = (-dy).atan2(dx).rem_euclid(std::f64::consts::TAU) / std::f64::consts::TAU;
        let used = used_share >= 1.0 || turn < used_share;
        match (used, side) {
            (true, false) => USED,
            (true, true) => USED_SIDE,
            (false, false) => FREE,
            (false, true) => FREE_SIDE,
        }
    };
    for layer in (0..=DEPTH).rev() {
        for y in -(RY as i32)..=(RY as i32) {
            let span = (RX * (1.0 - (f64::from(y) / RY).powi(2)).max(0.0).sqrt()) as i32;
            for x in -span..=span {
                let at = (center.0 + x, center.1 + y);
                p.fill_rgb(
                    Rect::new(at.0, at.1 + layer, 1, 1),
                    colour(at.0, at.1, layer != 0),
                );
            }
        }
    }
}

impl Page for FileProperties {
    fn title(&self) -> &'static str {
        // 标题随对象变化，由 `heading_title` 提供；这里给出窗口创建时的占位。
        "属性"
    }
    fn heading_title(&self) -> Option<String> {
        Some(match &self.body {
            Body::Drive(drive) => format!("{} 属性", drive.kind),
            Body::Entry(_) => format!("{} 属性", self.name),
        })
    }
    fn tabs(&self) -> &'static [&'static str] {
        &["常规"]
    }
    fn icon(&self) -> &'static str {
        "my-computer"
    }
    fn size(&self) -> (i32, i32) {
        (368, 424)
    }
    fn paint(&self, p: &mut Painter<'_>, page: Rect, _tab: usize, _now: i64, _icons: &Icons) {
        let ink = Ink::Color(C::ButtonText);
        heading(p, page, self.icon.as_ref(), &self.name);
        separator(p, page, page.y + 56);
        match &self.body {
            Body::Entry(entry) => {
                let step = p.line_height() + 6;
                let mut y = page.y + 68;
                line(p, page, &mut y, step, "类型:", &entry.kind);
                line(p, page, &mut y, step, "位置:", &entry.location);
                let cut = entry.contents.is_some_and(|(_, _, cut)| cut);
                let at_least = if cut { "至少 " } else { "" };
                line(
                    p,
                    page,
                    &mut y,
                    step,
                    "大小:",
                    &format!("{at_least}{}", size_text(entry.size)),
                );
                line(
                    p,
                    page,
                    &mut y,
                    step,
                    "占用空间:",
                    &format!("{at_least}{}", size_text(entry.on_disk)),
                );
                if let Some((files, folders, _)) = entry.contents {
                    line(
                        p,
                        page,
                        &mut y,
                        step,
                        "包含:",
                        &format!("{files} 个文件，{folders} 个文件夹"),
                    );
                }
                separator(p, page, y + 2);
                y += 14;
                for (label, time) in [
                    ("创建时间:", entry.created),
                    ("修改时间:", entry.modified),
                    ("访问时间:", entry.accessed),
                ] {
                    line(
                        p,
                        page,
                        &mut y,
                        step,
                        label,
                        &time.map(date_text).unwrap_or_default(),
                    );
                }
                separator(p, page, y + 2);
                p.text(page.x + 14, page.y + 292, "属性:", ink, false);
                let box_rect = Self::checkbox_rect(page);
                p.checkbox(box_rect.x, box_rect.y, self.readonly, false);
                p.text(box_rect.x + 20, box_rect.y - 2, "只读(R)", ink, false);
                p.checkbox(box_rect.x + 100, box_rect.y, entry.hidden, true);
                p.text(
                    box_rect.x + 120,
                    box_rect.y - 2,
                    "隐藏(H)",
                    Ink::Disabled,
                    false,
                );
            }
            Body::Drive(drive) => {
                let step = p.line_height() + 6;
                row(p, page, page.y + 68, "类型:", &drive.kind);
                row(p, page, page.y + 68 + step, "文件系统:", &drive.filesystem);
                separator(p, page, page.y + 68 + 2 * step + 2);
                let (used, free, total) = drive.capacity.unwrap_or_default();
                let mut y = page.y + 68 + 2 * step + 16;
                for (colour, label, value) in [(USED, "已用空间:", used), (FREE, "可用空间:", free)]
                {
                    p.fill_rgb(Rect::new(page.x + 14, y + 2, 10, 10), colour);
                    p.text_fit(page.x + 30, y, 76, label, ink, false);
                    p.text_fit(
                        page.x + 110,
                        y,
                        page.width - 122,
                        &size_text(value),
                        ink,
                        false,
                    );
                    y += step;
                }
                separator(p, page, y + 2);
                row(p, page, y + 14, "容量:", &size_text(total));
                let share = if total == 0 {
                    0.0
                } else {
                    used as f64 / total as f64
                };
                let center = (page.x + page.width / 2, y + 14 + step + 60);
                pie(p, center, share);
                p.text_centered(
                    Rect::new(page.x, center.1 + 46, page.width, p.line_height() + 4),
                    &format!("驱动器 {}", self.path.display()),
                    ink,
                    false,
                );
            }
        }
    }
    fn click(&mut self, page: Rect, _tab: usize, (x, y): (i32, i32)) -> bool {
        let rect = Self::checkbox_rect(page);
        let hit =
            (rect.x..rect.x + 70).contains(&x) && (rect.y - 2..rect.bottom() + 2).contains(&y);
        if hit && matches!(self.body, Body::Entry(_)) {
            self.readonly = !self.readonly;
            return true;
        }
        false
    }
    fn dirty(&self) -> bool {
        self.readonly != self.readonly_at_open
    }
    fn apply(&mut self) {
        if !self.dirty() {
            return;
        }
        let Ok(metadata) = fs::metadata(&self.path) else {
            return;
        };
        let mut permissions = metadata.permissions();
        let mode = permissions.mode();
        // Windows 的"只读"对应去掉所有写位，取消则恢复属主写位。
        permissions.set_mode(if self.readonly {
            mode & !0o222
        } else {
            mode | 0o200
        });
        if fs::set_permissions(&self.path, permissions).is_ok() {
            self.readonly_at_open = self.readonly;
        }
    }
}
