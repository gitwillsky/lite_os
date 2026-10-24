//! Win2000 视觉事实的唯一 owner：系统颜色、窗口度量与立体边框取色。
//!
//! 数据来自 `assets/classic/theme/classic.theme`，编译期嵌入作为默认值，运行时按搜索顺序被覆盖文件部分覆盖。
//! 键名沿用 `Control Panel\Colors` 与 `WindowMetrics` 的注册表值名，因此同一份数据也生成 Wine 前缀注册表。
//! 所有尺寸是 96 DPI 下的逻辑像素，乘以整数 scale 是调用者（`classic-ui`）的职责。

mod edge;
mod parse;
mod registry;

use std::{fmt, fs, io, path::Path};

pub use edge::{Edge, EdgeColors};
pub use parse::ParseError;

/// 8 位 RGB 颜色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rgb {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

macro_rules! system_colors {
    ($($variant:ident),* $(,)?) => {
        /// Win32 系统颜色；变体名即注册表值名。
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum SystemColor { $($variant),* }

        impl SystemColor {
            pub const ALL: &'static [SystemColor] = &[$(SystemColor::$variant),*];

            /// `Control Panel\Colors` 里的值名。
            pub const fn name(self) -> &'static str {
                match self { $(SystemColor::$variant => stringify!($variant)),* }
            }

            fn from_name(name: &str) -> Option<Self> {
                Self::ALL.iter().copied().find(|color| color.name() == name)
            }
        }
    };
}

system_colors!(
    Scrollbar,
    Background,
    ActiveTitle,
    InactiveTitle,
    Menu,
    Window,
    WindowFrame,
    MenuText,
    WindowText,
    TitleText,
    ActiveBorder,
    InactiveBorder,
    AppWorkspace,
    Hilight,
    HilightText,
    ButtonFace,
    ButtonShadow,
    GrayText,
    ButtonText,
    InactiveTitleText,
    ButtonHilight,
    ButtonDkShadow,
    ButtonLight,
    InfoText,
    InfoWindow,
    HotTrackingColor,
    GradientActiveTitle,
    GradientInactiveTitle,
);

macro_rules! metric_table {
    ($(#[$meta:meta])* $name:ident { $($field:ident => $key:literal),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
        pub struct $name { $(pub $field: i32),* }

        impl $name {
            /// `(键名, 值)` 列表，顺序与数据文件一致。
            pub fn entries(&self) -> Vec<(&'static str, i32)> {
                vec![$(($key, self.$field)),*]
            }

            fn slot(&mut self, key: &str) -> Option<&mut i32> {
                match key { $($key => Some(&mut self.$field),)* _ => None }
            }
        }
    };
}

metric_table!(
    /// `Control Panel\Desktop\WindowMetrics`：窗口部件度量（逻辑像素）。
    WindowMetrics {
        border_width => "BorderWidth",
        caption_height => "CaptionHeight",
        caption_width => "CaptionWidth",
        small_caption_height => "SmCaptionHeight",
        small_caption_width => "SmCaptionWidth",
        menu_height => "MenuHeight",
        menu_width => "MenuWidth",
        scroll_height => "ScrollHeight",
        scroll_width => "ScrollWidth",
        icon_spacing => "IconSpacing",
        icon_vertical_spacing => "IconVerticalSpacing",
    }
);

metric_table!(
    /// 任务栏度量（逻辑像素）；Win32 没有对应注册表项，由 desktop 独占使用。
    ShellMetrics {
        taskbar_height => "TaskbarHeight",
        taskbar_button_height => "TaskbarButtonHeight",
        taskbar_button_max_width => "TaskbarButtonMaxWidth",
        taskbar_button_gap => "TaskbarButtonGap",
    }
);

/// `SM_CXEDGE`：立体边框厚度，两层各 1 像素；由 `DrawEdge` 的几何固定，不随配色方案变化。
pub const EDGE_WIDTH: i32 = 2;
/// `SM_CXFIXEDFRAME`：不可缩放对话框的边框宽度。
pub const FIXED_FRAME_WIDTH: i32 = 3;

impl WindowMetrics {
    /// `SM_CXSIZEFRAME`：可缩放窗口边框 = 细边框 + 3 像素缩放带。
    pub fn size_frame(&self) -> i32 {
        self.border_width + 3
    }

    /// `SM_CYCAPTION`：标题栏高度含 1 像素分隔线。
    pub fn caption_total_height(&self) -> i32 {
        self.caption_height + 1
    }

    /// `SM_CYMENU`：菜单栏高度含 1 像素分隔线。
    pub fn menu_total_height(&self) -> i32 {
        self.menu_height + 1
    }
}

/// 完整主题：系统颜色与度量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    colors: [Rgb; SystemColor::ALL.len()],
    pub window: WindowMetrics,
    pub shell: ShellMetrics,
}

const DEFAULT_THEME: &str = include_str!("../../../assets/classic/theme/classic.theme");

/// 读取主题文件失败。
#[derive(Debug)]
pub enum LoadError {
    Read { path: String, source: io::Error },
    Parse { path: String, source: ParseError },
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => write!(f, "{path}: {source}"),
            Self::Parse { path, source } => write!(f, "{path}: {source}"),
        }
    }
}

impl std::error::Error for LoadError {}

impl Theme {
    /// 仓库内置的 Win2000 经典主题。
    pub fn classic() -> Self {
        let blank = Theme {
            colors: [Rgb::default(); SystemColor::ALL.len()],
            window: WindowMetrics::default(),
            shell: ShellMetrics::default(),
        };
        // 数据文件随二进制嵌入，单元测试保证它完整且合法，所以这里的失败只可能是构建产物损坏。
        blank
            .overlay(DEFAULT_THEME)
            .expect("embedded classic.theme must be valid")
    }

    /// 返回 `text` 中出现的键覆盖到 `self` 后的新主题；未出现的键保持不变。
    ///
    /// # Errors
    ///
    /// 未知节或键、缺少 `=`、颜色或度量值非法时返回带行号的 [`ParseError`]，`self` 不受影响。
    pub fn overlay(&self, text: &str) -> Result<Theme, ParseError> {
        parse::overlay(self.clone(), text)
    }

    /// 按顺序取第一个存在的主题文件，覆盖在内置默认之上；一个都不存在则返回内置默认。
    ///
    /// 搜索顺序即 用户 > 本地原版 > 仓库默认 的优先级，由调用者决定路径列表。
    ///
    /// # Errors
    ///
    /// 文件存在但不可读或内容非法；不会悄悄回退到下一个候选，避免掩盖用户配置错误。
    pub fn load(search: &[&Path]) -> Result<Theme, LoadError> {
        let classic = Theme::classic();
        for path in search {
            let text = match fs::read_to_string(path) {
                Ok(text) => text,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(LoadError::Read {
                        path: path.display().to_string(),
                        source,
                    });
                }
            };
            return classic.overlay(&text).map_err(|source| LoadError::Parse {
                path: path.display().to_string(),
                source,
            });
        }
        Ok(classic)
    }

    pub fn color(&self, color: SystemColor) -> Rgb {
        self.colors[color as usize]
    }
}

#[cfg(test)]
mod tests;
