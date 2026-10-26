//! 一个 scale 下解析好的系统字体集合；字体名、磅值与粗细全部来自主题的 `[Fonts]`。
use classic_theme::{LogicalFont, SystemFonts};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};
use typeface::{FontCatalog, Typeface};

/// 系统字体的用途，对应 `NONCLIENTMETRICS` 与图标字体。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontRole {
    Caption,
    SmallCaption,
    Menu,
    Status,
    /// 消息框与对话框正文；Painter 的默认字体。
    Message,
    Icon,
}

/// 自定义逻辑字体缓存上限；Web 视图标题等少量固定样式远低于此值。
const CUSTOM_LIMIT: usize = 16;

/// 解析后的系统字体与自定义字体入口；只在 UI 线程使用。
pub struct Fonts {
    caption: Typeface,
    small_caption: Typeface,
    menu: Typeface,
    status: Typeface,
    message: Typeface,
    icon: Typeface,
    catalog: Rc<FontCatalog>,
    scale: u32,
    smoothing: bool,
    // OWNER: 本集合独占自定义字体的解析结果，Rc 保活正在绘制的借用。
    // FAILURE: 不缓存会在每次绘制重新解析映射链；不限制条数会随应用请求的样式无限增长。
    custom: RefCell<BTreeMap<LogicalFont, Rc<Typeface>>>,
}

impl Fonts {
    /// 按主题字体与 scale 解析全部系统字体。
    pub fn new(catalog: Rc<FontCatalog>, system: &SystemFonts, scale: u32) -> Self {
        let resolve = |font: &LogicalFont| catalog.resolve(font, scale, system.smoothing);
        Self {
            caption: resolve(&system.caption),
            small_caption: resolve(&system.small_caption),
            menu: resolve(&system.menu),
            status: resolve(&system.status),
            message: resolve(&system.message),
            icon: resolve(&system.icon),
            smoothing: system.smoothing,
            catalog,
            scale,
            custom: RefCell::new(BTreeMap::new()),
        }
    }

    /// 某个用途的系统字体。
    pub fn role(&self, role: FontRole) -> &Typeface {
        match role {
            FontRole::Caption => &self.caption,
            FontRole::SmallCaption => &self.small_caption,
            FontRole::Menu => &self.menu,
            FontRole::Status => &self.status,
            FontRole::Message => &self.message,
            FontRole::Icon => &self.icon,
        }
    }

    /// 某个用途字体的逻辑描述，供派生样式（如 Web 视图的 `14pt menu`）只改磅值或粗细。
    pub fn logical(system: &SystemFonts, role: FontRole) -> &LogicalFont {
        match role {
            FontRole::Caption => &system.caption,
            FontRole::SmallCaption => &system.small_caption,
            FontRole::Menu => &system.menu,
            FontRole::Status => &system.status,
            FontRole::Message => &system.message,
            FontRole::Icon => &system.icon,
        }
    }

    /// 解析任意逻辑字体；同一描述重复使用缓存结果，超过上限时逐出最早插入的键序项。
    pub fn custom(&self, font: &LogicalFont) -> Rc<Typeface> {
        if let Some(found) = self.custom.borrow().get(font) {
            return found.clone();
        }
        let resolved = Rc::new(self.catalog.resolve(font, self.scale, self.smoothing));
        let mut cache = self.custom.borrow_mut();
        if cache.len() >= CUSTOM_LIMIT {
            cache.pop_first();
        }
        cache.insert(font.clone(), resolved.clone());
        resolved
    }
}
