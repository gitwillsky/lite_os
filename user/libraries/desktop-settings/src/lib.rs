//! 每用户的桌面设置文件：`$HOME/.config/liteos/desktop.conf`。
extern crate liteos_runtime as _;
use std::{
    fs,
    io::{self, Write},
    path::PathBuf,
    time::SystemTime,
};

/// 桌面设置；`Default` 是 Windows 2000 的出厂值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// 任务栏总在最前：关闭后窗口可以盖住任务栏。
    pub always_on_top: bool,
    /// 任务栏自动隐藏：不用时收成屏幕底边一条细线。
    pub auto_hide: bool,
    pub show_clock: bool,
    /// 桌面背景色；`None` 用主题的 `Background`。
    pub background: Option<[u8; 3]>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            always_on_top: true,
            auto_hide: false,
            show_clock: true,
            background: None,
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
                "background.color" => settings.background = parse_color(value.trim()),
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
        Self::update(|settings| *settings = *self).map(drop)
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
        if let Some([red, green, blue]) = self.background {
            text.push_str(&format!(
                "background.color=#{red:02x}{green:02x}{blue:02x}\n"
            ));
        }
        let mut temporary = None;
        for index in 0..128 {
            let candidate = path.with_extension(format!("{}.{}.new", std::process::id(), index));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
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
