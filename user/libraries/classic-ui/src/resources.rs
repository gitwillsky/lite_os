//! UI 上下文资源；应用不再拥有字体目录和按 scale 重复的加载分支。
use classic_theme::Theme;
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    rc::Rc,
};
use typeface::FontCatalog;

use crate::Fonts;

/// 一个 UI 上下文的主题和字体 owner；不修改进程环境，不跨线程共享。
pub struct Resources {
    theme: Theme,
    font_directory: PathBuf,
    /// 字体目录只在首次绘制时扫描一次；主题重载不需要重新读取字体文件。
    catalog: Option<Rc<FontCatalog>>,
    fonts: BTreeMap<i32, Rc<Fonts>>,
    generation: u64,
    terminal: Option<Rc<terminal_font::TerminalFont>>,
    icons: Option<crate::AppIcons>,
    large_icons: Option<crate::AppIcons>,
    images: BTreeMap<(String, i32), Option<Rc<crate::Bitmap>>>,
}
impl Resources {
    /// 使用系统主题与字体目录建立资源上下文，主题损坏或不可读取时返回错误。
    pub fn system() -> io::Result<Self> {
        Ok(Self {
            theme: Theme::load(&[Path::new("/etc/liteos/theme.theme")])
                .map_err(|error| io::Error::other(error.to_string()))?,
            font_directory: PathBuf::from("/usr/share/liteos/fonts"),
            catalog: None,
            fonts: BTreeMap::new(),
            generation: 1,
            terminal: None,
            icons: None,
            large_icons: None,
            images: BTreeMap::new(),
        })
    }
    /// 以已安装 desktop entry 的 app_id 取得共享图标，首次使用才读取；缺失时使用通用程序图标。
    pub fn application_icon(&mut self, app_id: &str) -> Option<Rc<crate::Bitmap>> {
        self.icons
            .get_or_insert_with(|| crate::AppIcons::load(16))
            .owned(app_id)
    }
    /// 读取系统界面图标，允许 16/32 逻辑像素与安全素材名；缺失返回 None，缓存最多 128 项。
    pub fn icon(&mut self, stem: &str, size: i32) -> Option<Rc<crate::Bitmap>> {
        if !matches!(size, 16 | 32) {
            return None;
        }
        self.image(stem, size)
    }
    /// 读取整张系统界面位图（如工具栏位图条 `toolbar-hot-1x`）；名称规则与缓存同 [`Resources::icon`]。
    pub fn bitmap(&mut self, name: &str) -> Option<Rc<crate::Bitmap>> {
        self.image(name, 0)
    }
    /// `size` 为 0 时读取 `<stem>.bmp`，否则 `<stem>-<size>.bmp`。
    fn image(&mut self, stem: &str, size: i32) -> Option<Rc<crate::Bitmap>> {
        if stem.is_empty()
            || stem.len() > 96
            || !stem
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return None;
        }
        let key = (stem.to_owned(), size);
        if let Some(image) = self.images.get(&key) {
            return image.clone();
        }
        let image = crate::Bitmap::load(&Path::new("/usr/share/liteos/ui").join(if size == 0 {
            format!("{stem}.bmp")
        } else {
            format!("{stem}-{size}.bmp")
        }))
        .ok()
        .map(Rc::new);
        // OWNER: context 缓存资源读取（包括缺失结果）；活跃控件的 Rc 不受逐出影响。
        // FAILURE: 无上限会随目录/应用历史增长；不缓存缺失会在每次刷新反复读取同一路径。
        if self.images.len() == 128 {
            self.images.pop_first();
        }
        self.images.insert(key, image.clone());
        image
    }
    /// 以已安装 desktop entry 的 app_id 查找 16/32 像素图标，缺失时使用通用程序图标。
    pub fn application_icon_size(&mut self, app_id: &str, size: i32) -> Option<Rc<crate::Bitmap>> {
        if size == 32 {
            self.large_icons
                .get_or_insert_with(|| crate::AppIcons::load(32))
                .owned(app_id)
        } else if size == 16 {
            self.application_icon(app_id)
        } else {
            None
        }
    }
    /// 按需读取系统等宽字体，只由资源上下文持有一份解析结果；资源缺失或损坏返回错误。
    pub fn terminal_font(&mut self) -> io::Result<Rc<terminal_font::TerminalFont>> {
        if let Some(font) = &self.terminal {
            return Ok(font.clone());
        }
        let font = Rc::new(terminal_font::TerminalFont::open()?);
        self.terminal = Some(font.clone());
        Ok(font)
    }
    /// 当前主题，只借用权威值；没有第二份应用侧可写镜像。
    pub fn theme(&self) -> &Theme {
        &self.theme
    }
    /// 当前资源代际，用于判断上次完成绘制后主题是否已更新。
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// 原子重新加载主题，成功后所有窗口在下一次 paint 时失效；错误保留原主题。
    pub fn reload(&mut self) -> io::Result<()> {
        let theme = Theme::load(&[Path::new("/etc/liteos/theme.theme")])
            .map_err(|error| io::Error::other(error.to_string()))?;
        let generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| io::Error::other("resource generations exhausted"))?;
        self.theme = theme;
        self.generation = generation;
        // 系统字体属于主题：重载后按新主题重新解析，字体目录本身保持。
        self.fonts.clear();
        Ok(())
    }
    /// 取得整数 scale 下按主题解析的系统字体；scale 非正或字体目录不可用返回错误。
    pub fn fonts(&mut self, scale: i32) -> io::Result<Rc<Fonts>> {
        if scale <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid font scale",
            ));
        }
        if let Some(fonts) = self.fonts.get(&scale) {
            return Ok(fonts.clone());
        }
        let catalog = match &self.catalog {
            Some(catalog) => catalog.clone(),
            None => {
                let catalog = Rc::new(
                    FontCatalog::load(&self.font_directory)
                        .map_err(|error| io::Error::other(error.to_string()))?,
                );
                self.catalog = Some(catalog.clone());
                catalog
            }
        };
        let fonts = Rc::new(Fonts::new(catalog, &self.theme.fonts, scale as u32));
        // OWNER: 资源上下文独占四个 scale 的缓存，Rc 保活当前绘制借用。
        // PROOF: 逐出只释放缓存引用；重绘不在持借用期间递归进入另一次绘制。
        // FAILURE: 不缓存会在每次绘制重新解析映射链；不限制条数会随输出缩放历史持续占用内存。
        if self.fonts.len() == 4 {
            self.fonts.pop_first();
        }
        self.fonts.insert(scale, fonts.clone());
        Ok(fonts)
    }
}
