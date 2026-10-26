//! Alt+Tab 切换面板：按住 Alt 期间的选择状态与 Windows 2000 风格的图标网格。
//!
//! 选择状态只存在于按住 Alt 的这段时间；松开 Alt 才真正激活窗口，期间窗口栈不变。

use canvas::Rect;
use classic_theme::{Edge, SystemColor as C};
use classic_ui::{AppIcons, Ink, Painter};

use wayland_wire::wire::ObjectId;

use crate::wayland::ClientId;

const MAX_COLUMNS: usize = 7;
const CELL: i32 = 43;
const MARGIN: i32 = 13;
const TITLE_HEIGHT: i32 = 24;

/// 面板里的一个窗口。
pub struct Entry {
    pub client: ClientId,
    pub surface: ObjectId,
    pub title: String,
    pub app_id: String,
}

/// 一次 Alt+Tab 会话；`entries` 按最近使用在前排列。
pub struct Switcher {
    entries: Vec<Entry>,
    selected: usize,
}

impl Switcher {
    /// 开始一次切换。`entries` 为空返回 `None`。
    ///
    /// 初始选中第二项（上一个用过的窗口）；Alt+Shift+Tab 开始则选中最后一项。只有一项时选中它自己。
    pub fn open(entries: Vec<Entry>, reverse: bool) -> Option<Self> {
        if entries.is_empty() {
            return None;
        }
        let mut switcher = Self {
            selected: 0,
            entries,
        };
        switcher.step(reverse);
        Some(switcher)
    }

    /// 选中下一项（`reverse` 为上一项），首尾相接。
    pub fn step(&mut self, reverse: bool) {
        let count = self.entries.len();
        self.selected = if reverse {
            (self.selected + count - 1) % count
        } else {
            (self.selected + 1) % count
        };
    }

    /// 松开 Alt 时要激活的窗口。
    pub fn target(&self) -> (ClientId, ObjectId) {
        let entry = &self.entries[self.selected];
        (entry.client, entry.surface)
    }
}

/// 面板用到的字体与图标；加载失败时面板不显示，切换本身照常进行。
pub struct Art {
    /// 按系统主题解析的字体；叠加层与应用使用同一份字体设置。
    pub fonts: std::rc::Rc<classic_ui::Fonts>,
    icons: AppIcons,
}

impl Art {
    /// 读取输出 `scale` 对应的字体与 32×32 程序图标。
    pub fn load(scale: u32) -> std::io::Result<Self> {
        let fonts = classic_ui::Resources::system()?.fonts(scale as i32)?;
        let icons = AppIcons::load(32);
        Ok(Self { fonts, icons })
    }
}

/// 把面板画在屏幕中央；`scale` 为输出 scale，几何以逻辑像素计。
pub fn paint(p: &mut Painter<'_>, art: &Art, screen: (i32, i32), switcher: &Switcher) {
    let (screen_w, screen_h) = screen;
    let scale = p.scale();
    let count = switcher.entries.len() as i32;
    let columns = count.min(MAX_COLUMNS as i32);
    let rows = (count + columns - 1) / columns;
    let width = columns * CELL + 2 * MARGIN;
    let height = rows * CELL + TITLE_HEIGHT + 2 * MARGIN;
    let panel = Rect::new(
        (screen_w / scale - width) / 2,
        (screen_h / scale - height) / 2,
        width,
        height,
    );
    let inner = p.edge(panel, Edge::Raised);
    p.fill(inner, C::ButtonFace);
    for (index, entry) in switcher.entries.iter().enumerate() {
        let (column, row) = ((index % MAX_COLUMNS) as i32, (index / MAX_COLUMNS) as i32);
        let cell = Rect::new(
            panel.x + MARGIN + column * CELL,
            panel.y + MARGIN + row * CELL,
            CELL,
            CELL,
        );
        if index == switcher.selected {
            // 选中项：两像素的深色方框。
            p.outline(cell.inset(1), C::WindowFrame);
            p.outline(cell.inset(2), C::WindowFrame);
        }
        art.icons.paint(
            p,
            &entry.app_id,
            cell.x + (CELL - 32) / 2,
            cell.y + (CELL - 32) / 2,
        );
    }
    let title = &switcher.entries[switcher.selected].title;
    let area = Rect::new(
        panel.x + MARGIN,
        panel.bottom() - MARGIN - TITLE_HEIGHT + 6,
        width - 2 * MARGIN,
        p.line_height(),
    );
    let shown = p.fit(title, area.width);
    p.text_centered(area, &shown, Ink::Color(C::ButtonText), false);
}
