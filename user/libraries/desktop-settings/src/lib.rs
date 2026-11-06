//! 每用户的桌面设置文件：`$HOME/.config/liteos/desktop.conf`。
extern crate liteos_runtime as _;
use std::{
    collections::BTreeSet,
    fs,
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    time::SystemTime,
};

/// 资源管理器为一个位置保存的查看方式。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FolderView {
    #[default]
    LargeIcons,
    SmallIcons,
    List,
    Details,
}

/// 保存于文件夹视图中的一列。字段标识由该视图的 owner 定义。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FolderColumn {
    /// 视图定义的稳定列 ID。
    pub id: u64,
    /// 逻辑像素宽度。
    pub width: i32,
    /// 是否参与详细资料视图。
    pub visible: bool,
}

impl FolderView {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "large-icons" => Self::LargeIcons,
            "small-icons" => Self::SmallIcons,
            "list" => Self::List,
            "details" => Self::Details,
            _ => return None,
        })
    }

    fn key(self) -> &'static str {
        match self {
            Self::LargeIcons => "large-icons",
            Self::SmallIcons => "small-icons",
            Self::List => "list",
            Self::Details => "details",
        }
    }
}

/// 墙纸显示方式；对应 `HKCU\Control Panel\Desktop` 的 TileWallpaper=1（平铺）、
/// WallpaperStyle=2（拉伸），其余为居中。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WallpaperStyle {
    #[default]
    Center,
    Tile,
    Stretch,
}

impl WallpaperStyle {
    fn key(self) -> &'static str {
        match self {
            Self::Center => "center",
            Self::Tile => "tile",
            Self::Stretch => "stretch",
        }
    }
}

/// 桌面设置；`Default` 是 Windows 2000 的出厂值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// 任务栏总在最前：关闭后窗口可以盖住任务栏。
    pub always_on_top: bool,
    /// 任务栏自动隐藏：不用时收成屏幕底边一条细线。
    pub auto_hide: bool,
    pub show_clock: bool,
    /// 桌面背景色；`None` 用主题的 `Background`。
    pub background: Option<[u8; 3]>,
    /// 墙纸图片的绝对路径；`None` 即“(无)”。路径必须是不含换行的 UTF-8，否则不写入文件。
    pub wallpaper: Option<PathBuf>,
    pub wallpaper_style: WallpaperStyle,
    folder_views: Vec<(String, FolderView)>,
    folder_columns: Vec<(String, Vec<FolderColumn>)>,
    hidden_network_icons: BTreeSet<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            always_on_top: true,
            auto_hide: false,
            show_clock: true,
            background: None,
            wallpaper: None,
            wallpaper_style: WallpaperStyle::Center,
            folder_views: Vec::new(),
            folder_columns: Vec::new(),
            hidden_network_icons: BTreeSet::new(),
        }
    }
}

fn parse_color(value: &str) -> Option<[u8; 3]> {
    let value = value.strip_prefix('#').unwrap_or(value);
    if value.len() != 6 {
        return None;
    }
    let number = u32::from_str_radix(value, 16).ok()?;
    Some([(number >> 16) as u8, (number >> 8) as u8, number as u8])
}

impl Settings {
    /// 取该位置的列顺序、宽度和可见性。空切片表示使用默认列。
    pub fn folder_columns(&self, location: &str) -> &[FolderColumn] {
        self.folder_columns
            .iter()
            .find(|(key, _)| key == location)
            .map_or(&[], |(_, columns)| columns)
    }

    /// 更新该位置的列布局，空布局移除已有偏好。
    pub fn set_folder_columns(&mut self, location: String, columns: Vec<FolderColumn>) {
        if location.is_empty()
            || location.bytes().any(|byte| byte.is_ascii_control())
            || columns.len() > 32
            || columns.iter().enumerate().any(|(index, column)| {
                column.id == 0
                    || !(24..=32768).contains(&column.width)
                    || columns[..index]
                        .iter()
                        .any(|earlier| earlier.id == column.id)
            })
        {
            return;
        }
        self.folder_columns.retain(|(key, _)| key != &location);
        if !columns.is_empty() {
            if self.folder_columns.len() == 512 {
                self.folder_columns.remove(0);
            }
            self.folder_columns.push((location, columns));
        }
    }
    /// 返回 `interface` 是否参与通知区域显示。无用户设置的连接默认显示。
    pub fn network_icon_visible(&self, interface: &str) -> bool {
        !self.hidden_network_icons.contains(interface)
    }

    /// 设置 `interface` 是否参与通知区域显示。空名和含控制字符的名称忽略。
    pub fn set_network_icon_visible(&mut self, interface: String, visible: bool) {
        if visible {
            self.hidden_network_icons.remove(&interface);
        } else if !interface.is_empty() && !interface.bytes().any(|byte| byte.is_ascii_control()) {
            self.hidden_network_icons.insert(interface);
        }
    }

    /// 读取 `location` 的查看方式。未设置时使用大图标。
    pub fn folder_view(&self, location: &str) -> FolderView {
        self.folder_views
            .iter()
            .find(|(key, _)| key == location)
            .map(|(_, view)| *view)
            .unwrap_or_default()
    }

    /// 更新 `location` 的查看方式；键是本地文件 URI 或外壳位置标识。
    pub fn set_folder_view(&mut self, location: String, view: FolderView) {
        if location.is_empty() || location.bytes().any(|byte| byte.is_ascii_control()) {
            return;
        }
        self.folder_views.retain(|(key, _)| key != &location);
        if view != FolderView::default() {
            // 只保留最近设置过的 512 个位置，防止设置文件随历史浏览无限增长。
            if self.folder_views.len() == 512 {
                self.folder_views.remove(0);
            }
            self.folder_views.push((location, view));
        }
    }

    /// 设置文件路径；没有 `HOME` 时为 `None`。
    pub fn path() -> Option<PathBuf> {
        Some(PathBuf::from(std::env::var_os("HOME")?).join(".config/liteos/desktop.conf"))
    }

    /// 读取设置；文件不存在、无法读取或某项不合法时，那一项保持默认。
    pub fn load() -> Self {
        let mut settings = Self::default();
        let Some(text) = Self::path().and_then(|path| fs::read_to_string(path).ok()) else {
            return settings;
        };
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let flag = || Some(value.trim() == "1");
            match key.trim() {
                "taskbar.always_on_top" => settings.always_on_top = flag().unwrap_or(true),
                "taskbar.auto_hide" => settings.auto_hide = flag().unwrap_or(false),
                "taskbar.show_clock" => settings.show_clock = flag().unwrap_or(true),
                "taskbar.hide_network" => {
                    settings.set_network_icon_visible(value.to_owned(), false)
                }
                "background.color" => settings.background = parse_color(value.trim()),
                "background.wallpaper" => {
                    let path = PathBuf::from(value.trim());
                    settings.wallpaper = path.is_absolute().then_some(path);
                }
                "background.wallpaper_style" => {
                    settings.wallpaper_style = match value.trim() {
                        "tile" => WallpaperStyle::Tile,
                        "stretch" => WallpaperStyle::Stretch,
                        _ => WallpaperStyle::Center,
                    }
                }
                "explorer.view" => {
                    if let Some((location, style)) = value.split_once('\t')
                        && let Some(style) = FolderView::parse(style)
                    {
                        settings.set_folder_view(location.to_owned(), style);
                    }
                }
                "explorer.columns" => {
                    if let Some((location, columns)) = value.split_once('\t') {
                        let columns = columns
                            .split(',')
                            .map(|column| {
                                let mut fields = column.split(':');
                                let id = fields.next()?.parse().ok()?;
                                let width = fields.next()?.parse().ok()?;
                                let visible = match fields.next()? {
                                    "0" => false,
                                    "1" => true,
                                    _ => return None,
                                };
                                fields.next().is_none().then_some(FolderColumn {
                                    id,
                                    width,
                                    visible,
                                })
                            })
                            .collect::<Option<Vec<_>>>();
                        if let Some(columns) = columns {
                            settings.set_folder_columns(location.to_owned(), columns);
                        }
                    }
                }
                _ => {}
            }
        }
        settings
    }

    /// 写回设置文件（先建目录）。
    ///
    /// # Errors
    ///
    /// 没有 `HOME`、目录不可写等 I/O 错误。
    pub fn save(&self) -> io::Result<()> {
        Self::update(|settings| *settings = self.clone()).map(drop)
    }
    /// 在标准 flock 保护下重读并更新当前文件，原子发布；闭包只修改所属领域的字段。
    /// 错误保留旧文件，锁随 OFD 析构释放；本操作必须在后台执行。
    pub fn update(change: impl FnOnce(&mut Self)) -> io::Result<Self> {
        let path = Self::path().ok_or_else(|| io::Error::other("HOME missing"))?;
        let directory = path
            .parent()
            .ok_or_else(|| io::Error::other("settings directory missing"))?;
        fs::create_dir_all(directory)?;
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(directory.join("desktop.conf.lock"))?;
        // OWNER: flock 关联此 OFD，串行化跨进程读改写；无锁会丢失同时应用的显示/任务栏设置。
        // PROOF: I/O、闭包和原子 rename 都在文件锁期间完成，任意错误/退出由 File Drop 解锁。
        lock.lock()?;
        let mut settings = Self::load();
        change(&mut settings);
        settings.publish(&path)?;
        Ok(settings)
    }
    fn publish(&self, path: &std::path::Path) -> io::Result<()> {
        let flag = |value: bool| u8::from(value);
        let mut text = format!(
            "taskbar.always_on_top={}\ntaskbar.auto_hide={}\ntaskbar.show_clock={}\n",
            flag(self.always_on_top),
            flag(self.auto_hide),
            flag(self.show_clock)
        );
        for interface in &self.hidden_network_icons {
            use std::fmt::Write as _;
            writeln!(text, "taskbar.hide_network={interface}").expect("writing to a string");
        }
        if let Some([red, green, blue]) = self.background {
            text.push_str(&format!(
                "background.color=#{red:02x}{green:02x}{blue:02x}\n"
            ));
        }
        if let Some(path) = self
            .wallpaper
            .as_deref()
            .and_then(|path| path.to_str())
            .filter(|path| !path.contains('\n'))
        {
            text.push_str(&format!("background.wallpaper={path}\n"));
        }
        text.push_str(&format!(
            "background.wallpaper_style={}\n",
            self.wallpaper_style.key()
        ));
        for (location, view) in &self.folder_views {
            use std::fmt::Write as _;
            writeln!(text, "explorer.view={location}\t{}", view.key())
                .expect("writing to a string");
        }
        for (location, columns) in &self.folder_columns {
            use std::fmt::Write as _;
            write!(text, "explorer.columns={location}\t").expect("writing to a string");
            for (index, column) in columns.iter().enumerate() {
                if index != 0 {
                    text.push(',');
                }
                write!(
                    text,
                    "{}:{}:{}",
                    column.id,
                    column.width,
                    u8::from(column.visible)
                )
                .expect("writing to a string");
            }
            text.push('\n');
        }
        let mut temporary = None;
        for index in 0..128 {
            let candidate = path.with_extension(format!("{}.{}.new", std::process::id(), index));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                // 设置可包含用户访问过的路径。
                .mode(0o600)
                .open(&candidate)
            {
                Ok(file) => {
                    temporary = Some((candidate, file));
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        let (temporary, mut file) = temporary.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "settings temporary files exhausted",
            )
        })?;
        let result = (|| {
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temporary, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }

    /// 设置文件的修改时间；桌面据此发现控制面板改了设置。文件不存在为 `None`。
    pub fn stamp() -> Option<SystemTime> {
        fs::metadata(Self::path()?).ok()?.modified().ok()
    }
}
