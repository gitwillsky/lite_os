//! 资源管理器窗口的布局、交互状态与绘制；坐标均为逻辑像素。
use crate::{
    browser::Browser,
    chrome::{self, Entry, MENU_TITLES, TOOLS},
    grid::{Grid, HEADER_HEIGHT, INITIAL_COLUMNS, ViewMode, inside},
    model::{Icon, Item, date_text, size_text},
};
use canvas::Rect;
use classic_theme::{Edge, SystemColor as C};
use classic_ui::{Bitmap, ColumnHeader, Edit, Ink, MenuItemState, Painter, Scroll, Tooltip};
use std::path::Path;

pub const MENU_HEIGHT: i32 = 20;
const TOOLBAR_HEIGHT: i32 = 44;
const ADDRESS_HEIGHT: i32 = 28;
const STATUS_HEIGHT: i32 = 22;
const PANE_WIDTH: i32 = 190;
const SPLITTER: i32 = 4;
const TREE_HEADER: i32 = 18;
const TREE_ROW: i32 = 18;
const ITEM_HEIGHT: i32 = 20;

/// 已加载的原版图标；缺失的素材（只提交了默认素材的构建）按"无图标"绘制。
pub struct Assets {
    small: Vec<(Icon, Option<Bitmap>)>,
    pub(crate) large: Vec<(Icon, Option<Bitmap>)>,
    folder_open: Option<Bitmap>,
    pub(crate) question: Option<Bitmap>,
    pub(crate) error: Option<Bitmap>,
    /// 默认 / 悬停 / 禁用 三条 47 帧工具栏位图。
    toolbar: [Option<Bitmap>; 3],
}

impl Assets {
    pub fn load(directory: &Path) -> Self {
        let load = |name: String| Bitmap::load(&directory.join(name)).ok();
        let icons = [
            Icon::MyComputer,
            Icon::Folder,
            Icon::File,
            Icon::Text,
            Icon::Application,
            Icon::System,
            Icon::Disk,
            Icon::OpticalDisc,
            Icon::ControlPanel,
            Icon::MyDocuments,
            Icon::RecycleBin,
            Icon::DateTime,
            Icon::Sound,
            Icon::DisplaySettings,
            Icon::Users,
            Icon::NetworkPlaces,
            Icon::Network,
            Icon::DeviceManager,
        ];
        let by_size = |size: u32| {
            icons
                .iter()
                .map(|icon| (*icon, load(format!("{}-{size}.bmp", icon.file_stem()))))
                .collect()
        };
        Self {
            small: by_size(16),
            large: by_size(32),
            folder_open: load("folder-open-16.bmp".into()),
            question: load("msg-question-32.bmp".into()),
            error: load("msg-error-32.bmp".into()),
            toolbar: ["default", "hot", "disabled"]
                .map(|name| load(format!("toolbar-{name}-1x.bmp"))),
        }
    }

    pub(crate) fn icon(set: &[(Icon, Option<Bitmap>)], icon: Icon) -> Option<&Bitmap> {
        set.iter().find(|(i, _)| *i == icon)?.1.as_ref()
    }
}

/// 弹出菜单。
pub struct Popup {
    pub origin: (i32, i32),
    /// 按条目内容算出的宽度（逻辑像素）；创建时为 0，由 [`Popup::measure`] 在事件处理后补上。
    pub width: i32,
    pub entries: Vec<Entry>,
    pub hover: Option<usize>,
    /// 由哪个顶层菜单打开；工具栏与右键打开的为 `None`。
    pub menu: Option<usize>,
    /// 是否由工具栏"查看"按钮打开（按钮在菜单打开期间保持按下外观）。
    pub from_toolbar: bool,
}

impl Popup {
    fn row_height(entry: &Entry) -> i32 {
        match entry {
            Entry::Item { .. } => ITEM_HEIGHT,
            Entry::Separator => 8,
        }
    }

    /// 按条目标签与快捷键量出宽度；条目变化后重新调用。
    pub fn measure(&mut self, p: &Painter<'_>) {
        self.width = p.menu_width(self.entries.iter().filter_map(|entry| match entry {
            Entry::Item {
                label, shortcut, ..
            } => Some((*label, *shortcut)),
            Entry::Separator => None,
        }));
    }

    pub fn rect(&self) -> Rect {
        let height: i32 = self.entries.iter().map(Self::row_height).sum();
        Rect::new(self.origin.0, self.origin.1, self.width, height + 4)
    }

    /// 命中的可点击行。
    pub fn row_at(&self, point: (i32, i32)) -> Option<usize> {
        let mut y = self.origin.1 + 2;
        for (index, entry) in self.entries.iter().enumerate() {
            let height = Self::row_height(entry);
            let row = Rect::new(self.origin.0 + 2, y, self.width - 4, height);
            if inside(row, point) && matches!(entry, Entry::Item { .. }) {
                return Some(index);
            }
            y += height;
        }
        None
    }
}

/// 窗口内随输入变化的界面状态。
pub struct Ui {
    pub pointer: (i32, i32),
    pub popup: Option<Popup>,
    pub address: Edit,
    pub address_focus: bool,
    /// 工具栏按钮的悬停提示，目标是 `TOOLS` 的序号。
    pub tooltip: Tooltip,
    /// 是否显示助记键下划线：按过 Alt 或 Tab 之后一直显示（Windows 2000 的键盘提示规则）。
    pub cues: bool,
    /// 键盘选中的菜单栏标题（单按 Alt 进入）。
    pub menu_focus: Option<usize>,
    /// Alt 按下后还没有别的按键：此时松开 Alt 才算"单独的 Alt"。
    pub alt_alone: bool,
    pub list_scroll: Scroll,
    /// "详细信息"视图各列的宽度与表头拖动状态。
    pub columns: ColumnHeader,
    pub tree_scroll: Scroll,
    /// 正被按住的工具栏按钮。
    pub pressed_tool: Option<usize>,
    /// 正在原地改名的项目序号与编辑框。
    pub rename: Option<(usize, Edit)>,
    /// 等待用户确认的危险操作。
    pub confirm: Option<crate::dialogs::Confirm>,
    /// 等待用户确认的失败提示。
    pub notice: Option<crate::dialogs::Notice>,
}

impl Ui {
    pub fn new(address: &str) -> Self {
        Self {
            pointer: (-1, -1),
            popup: None,
            address: Edit::new(address),
            address_focus: false,
            tooltip: Tooltip::default(),
            cues: false,
            menu_focus: None,
            alt_alone: false,
            list_scroll: Scroll::default(),
            columns: ColumnHeader::new(INITIAL_COLUMNS),
            tree_scroll: Scroll::default(),
            pressed_tool: None,
            rename: None,
            confirm: None,
            notice: None,
        }
    }
}

/// 窗口客户区的各个区域。
pub struct Layout {
    pub menu: Rect,
    pub toolbar: Option<Rect>,
    pub address: Rect,
    pub combo: Rect,
    pub go: Rect,
    /// 左侧面板（信息面板或文件夹树），宽度不足时没有。
    pub pane: Option<Rect>,
    /// 列表外框（含凹陷边）与其内部。
    pub list: Rect,
    pub list_inner: Rect,
    pub status: Option<Rect>,
}

pub fn layout(client: Rect, browser: &Browser) -> Layout {
    let menu = Rect::new(client.x, client.y, client.width, MENU_HEIGHT);
    let mut y = menu.bottom();
    let toolbar = browser.show_toolbar.then(|| {
        let rect = Rect::new(client.x, y + 2, client.width, TOOLBAR_HEIGHT);
        y = rect.bottom();
        rect
    });
    if toolbar.is_none() {
        y += 2;
    }
    let address = Rect::new(client.x, y, client.width, ADDRESS_HEIGHT);
    let combo = Rect::new(client.x + 58, y + 4, (client.width - 58 - 66).max(0), 21);
    let go = Rect::new(client.right() - 58, y + 4, 52, 21);
    let status = browser.show_status_bar.then(|| {
        Rect::new(
            client.x,
            client.bottom() - STATUS_HEIGHT,
            client.width,
            STATUS_HEIGHT,
        )
    });
    let body_top = address.bottom();
    let body_bottom = status.map_or(client.bottom(), |s| s.y);
    let body_height = (body_bottom - body_top).max(0);
    let pane =
        (client.width >= 420).then(|| Rect::new(client.x, body_top, PANE_WIDTH, body_height));
    let list_x = pane.map_or(client.x, |p| p.right() + SPLITTER);
    let list = Rect::new(list_x, body_top, client.right() - list_x, body_height);
    Layout {
        menu,
        toolbar,
        address,
        combo,
        go,
        pane,
        list,
        list_inner: list.inset(2),
        status,
    }
}

/// 顶层菜单标题的位置。
pub fn menu_title_rects(p: &Painter<'_>, menu: Rect) -> Vec<Rect> {
    let mut x = menu.x + 2;
    MENU_TITLES
        .iter()
        .map(|title| {
            let width = logical(p, p.text_width(title, false)) + 16;
            let rect = Rect::new(x, menu.y, width, menu.height);
            x += width;
            rect
        })
        .collect()
}

fn logical(p: &Painter<'_>, physical: i32) -> i32 {
    (physical + p.scale() - 1) / p.scale()
}

pub(crate) fn draw(bitmap: Option<&Bitmap>, p: &mut Painter<'_>, x: i32, y: i32) {
    if let Some(bitmap) = bitmap {
        bitmap.paint(p, x, y);
    }
}

pub struct Scene<'a> {
    pub browser: &'a Browser,
    pub ui: &'a Ui,
    pub assets: &'a Assets,
    pub active: bool,
}

pub fn paint(p: &mut Painter<'_>, scene: &Scene<'_>, client: Rect) {
    let Scene { browser, ui, .. } = scene;
    p.set_cues(ui.cues);
    let l = layout(client, browser);
    p.fill(client, C::ButtonFace);
    paint_menu_bar(p, scene, &l);
    if let Some(toolbar) = l.toolbar {
        p.edge(
            Rect::new(client.x, toolbar.y - 2, client.width, 2),
            Edge::Etched,
        );
        paint_toolbar(p, scene, toolbar);
    }
    p.edge(
        Rect::new(client.x, l.address.y - 2, client.width, 2),
        Edge::Etched,
    );
    paint_address(p, scene, &l);
    if let Some(pane) = l.pane {
        if browser.show_folders {
            paint_tree(p, scene, pane);
        } else {
            paint_info(p, scene, pane);
        }
    }
    paint_list(p, scene, &l);
    if let Some(status) = l.status {
        paint_status(p, scene, status);
    }
    if let Some(popup) = &ui.popup {
        paint_popup(p, popup);
    } else if let (Some(index), Some(bar)) = (ui.tooltip.visible(), l.toolbar) {
        // 提示出现在按钮正下方，收在客户区内。
        let tip = crate::chrome::TOOLS[index].tip;
        let button = crate::chrome::tool_rects(bar)[index];
        let width = p.text_width(tip, false) / p.scale() + 6;
        p.tooltip(
            button.x.min(client.right() - width),
            button.bottom() + 2,
            tip,
        );
    }
    if let Some(confirm) = &ui.confirm {
        crate::dialogs::paint_confirm(p, scene, confirm, client);
    }
    if let Some(notice) = &ui.notice {
        crate::dialogs::paint_notice(p, scene, notice, client);
    }
}

fn paint_menu_bar(p: &mut Painter<'_>, scene: &Scene<'_>, l: &Layout) {
    let open = scene.ui.popup.as_ref().and_then(|popup| popup.menu);
    for (index, (title, rect)) in MENU_TITLES
        .iter()
        .zip(menu_title_rects(p, l.menu))
        .enumerate()
    {
        let hovered = inside(rect, scene.ui.pointer);
        p.menu_bar_item(
            rect,
            title,
            hovered && open.is_none() || open == Some(index) || scene.ui.menu_focus == Some(index),
            open == Some(index),
        );
    }
}

fn paint_toolbar(p: &mut Painter<'_>, scene: &Scene<'_>, bar: Rect) {
    let Scene {
        browser,
        ui,
        assets,
        ..
    } = scene;
    for (index, (tool, rect)) in TOOLS.iter().zip(chrome::tool_rects(bar)).enumerate() {
        let enabled = chrome::tool_enabled(tool, browser);
        let hovered = enabled && inside(rect, ui.pointer) && ui.popup.is_none();
        let pressed = hovered && ui.pressed_tool == Some(index)
            || ui.popup.as_ref().is_some_and(|popup| popup.from_toolbar)
                && tool.command == Some(chrome::Command::ViewMenu);
        if pressed {
            p.edge(rect, Edge::SunkenOuter);
        } else if hovered {
            p.edge(rect, Edge::RaisedInner);
        }
        let strip = &assets.toolbar[if !enabled {
            2
        } else if hovered || pressed {
            1
        } else {
            0
        }];
        let shift = i32::from(pressed);
        let icon_y = if tool.label.is_empty() {
            rect.y + (rect.height - 20) / 2
        } else {
            rect.y + 4
        };
        if let Some(strip) = strip {
            strip.paint_region(
                p,
                rect.x + (rect.width - 20) / 2 + shift,
                icon_y + shift,
                tool.frame * 20,
                20,
            );
        }
        if !tool.label.is_empty() {
            let ink = if enabled {
                Ink::Color(C::ButtonText)
            } else {
                Ink::Disabled
            };
            p.text_centered(
                Rect::new(rect.x + shift, rect.y + 26 + shift, rect.width, 13),
                tool.label,
                ink,
                false,
            );
        }
    }
}

fn paint_address(p: &mut Painter<'_>, scene: &Scene<'_>, l: &Layout) {
    let Scene {
        browser,
        ui,
        assets,
        ..
    } = scene;
    p.text(
        6,
        l.combo.y + (l.combo.height - p.line_height()) / 2,
        "地址(D)",
        Ink::Color(C::ButtonText),
        false,
    );
    p.edit_with_leading(
        l.combo,
        &ui.address,
        ui.address_focus && scene.active,
        false,
        20,
    );
    draw(
        Assets::icon(&assets.small, browser.location.icon()),
        p,
        l.combo.x + 3,
        l.combo.y + 3,
    );
    // 下拉箭头按钮：画在编辑框右端内部。
    let arrow = Rect::new(l.combo.right() - 18, l.combo.y + 2, 16, l.combo.height - 4);
    let face = p.edge(arrow, Edge::Raised);
    p.fill(face, C::ButtonFace);
    p.fill(Rect::new(arrow.x + 5, arrow.y + 7, 7, 1), C::ButtonText);
    p.fill(Rect::new(arrow.x + 6, arrow.y + 8, 5, 1), C::ButtonText);
    p.fill(Rect::new(arrow.x + 7, arrow.y + 9, 3, 1), C::ButtonText);
    p.fill(Rect::new(arrow.x + 8, arrow.y + 10, 1, 1), C::ButtonText);
    let pressed = inside(l.go, ui.pointer) && ui.pressed_tool == Some(usize::MAX);
    p.push_button(
        l.go,
        "转到",
        classic_ui::ButtonState {
            pressed,
            ..Default::default()
        },
    );
}

/// 信息面板：当前位置的名称，以及选中项的说明。
fn paint_info(p: &mut Painter<'_>, scene: &Scene<'_>, pane: Rect) {
    let Scene {
        browser, assets, ..
    } = scene;
    p.fill(pane, C::Window);
    p.clipped(pane, |p| {
        draw(
            Assets::icon(&assets.large, browser.location.icon()),
            p,
            pane.x + 12,
            pane.y + 14,
        );
        p.text(
            pane.x + 12,
            pane.y + 54,
            &p.fit(&browser.location.title(), pane.width - 20),
            Ink::Color(C::WindowText),
            true,
        );
        p.fill(
            Rect::new(pane.x + 12, pane.y + 76, pane.width - 24, 2),
            C::Hilight,
        );
        let mut lines = Vec::new();
        match browser.selected.and_then(|i| browser.items.get(i)) {
            Some(item) => {
                lines.push(item.name.clone());
                lines.push(format!("类型：{}", item.kind));
                if let Some(size) = item.size {
                    lines.push(format!("大小：{}", size_text(size)));
                }
                if let Some(modified) = item.modified {
                    lines.push(format!("修改日期：{}", date_text(modified)));
                }
            }
            None => lines.push("选择一个项目可查看其说明。".into()),
        }
        let step = p.line_height() + 4;
        for (row, line) in lines.iter().enumerate() {
            let bold = row == 0 && browser.selected.is_some();
            p.text(
                pane.x + 12,
                pane.y + 88 + row as i32 * step,
                &p.fit(line, pane.width - 20),
                Ink::Color(C::WindowText),
                bold,
            );
        }
    });
}

/// 文件夹树：顶部"文件夹"标题条，下面是可展开的树。
pub fn tree_body(pane: Rect) -> Rect {
    Rect::new(
        pane.x,
        pane.y + TREE_HEADER,
        pane.width,
        pane.height - TREE_HEADER,
    )
}

/// 文件夹树的滚动几何（含滚动条）。
pub fn tree_scroll_geometry(pane: Rect, rows: usize) -> (Rect, Option<Rect>, i32) {
    let body = tree_body(pane);
    let content = rows as i32 * TREE_ROW;
    if content > body.height {
        let bar = Rect::new(
            body.right() - classic_ui::SCROLLBAR_WIDTH,
            body.y,
            classic_ui::SCROLLBAR_WIDTH,
            body.height,
        );
        (
            Rect::new(body.x, body.y, body.width - bar.width, body.height),
            Some(bar),
            content,
        )
    } else {
        (body, None, content)
    }
}

/// 树行在滚动后的矩形；展开框占前 16 逻辑像素之后的位置由 `expander_rect` 给出。
pub fn tree_row_rect(view: Rect, index: usize, scroll: i32) -> Rect {
    Rect::new(
        view.x,
        view.y + index as i32 * TREE_ROW - scroll,
        view.width,
        TREE_ROW,
    )
}

pub fn expander_rect(row: Rect, depth: i32) -> Rect {
    Rect::new(row.x + 4 + depth * 16, row.y + 4, 9, 9)
}

pub fn tree_close_rect(pane: Rect) -> Rect {
    Rect::new(pane.right() - 18, pane.y + 2, 14, 14)
}

fn paint_tree(p: &mut Painter<'_>, scene: &Scene<'_>, pane: Rect) {
    let Scene {
        browser,
        ui,
        assets,
        ..
    } = scene;
    p.fill(
        Rect::new(pane.x, pane.y, pane.width, TREE_HEADER),
        C::ButtonFace,
    );
    p.text(
        pane.x + 6,
        pane.y + (TREE_HEADER - p.line_height()) / 2,
        "文件夹",
        Ink::Color(C::ButtonText),
        false,
    );
    let close = tree_close_rect(pane);
    p.text_centered(close, "×", Ink::Color(C::ButtonText), true);
    let (view, bar, content) = tree_scroll_geometry(pane, browser.rows.len());
    p.fill(tree_body(pane), C::Window);
    let scroll = ui.tree_scroll.position;
    p.clipped(view, |p| {
        for (index, row) in browser.rows.iter().enumerate() {
            let rect = tree_row_rect(view, index, scroll);
            if rect.bottom() < view.y || rect.y > view.bottom() {
                continue;
            }
            let current = row.location == browser.location;
            let indent = rect.x + 4 + row.depth * 16;
            if row.expandable {
                let box_rect = expander_rect(rect, row.depth);
                p.outline(box_rect, C::ButtonShadow);
                p.fill(box_rect.inset(1), C::Window);
                p.fill(
                    Rect::new(box_rect.x + 2, box_rect.y + 4, 5, 1),
                    C::WindowText,
                );
                if !row.expanded {
                    p.fill(
                        Rect::new(box_rect.x + 4, box_rect.y + 2, 1, 5),
                        C::WindowText,
                    );
                }
            }
            let icon = if current {
                assets
                    .folder_open
                    .as_ref()
                    .filter(|_| matches!(row.icon, Icon::Folder))
            } else {
                None
            };
            draw(
                icon.or(Assets::icon(&assets.small, row.icon)),
                p,
                indent + 14,
                rect.y + 1,
            );
            let text_x = indent + 14 + 20;
            let label = p.fit(&row.label, (view.right() - text_x - 2).max(0));
            let width = logical(p, p.text_width(&label, false));
            let text_y = rect.y + (TREE_ROW - p.line_height()) / 2;
            if current {
                p.fill(
                    Rect::new(text_x - 2, rect.y + 1, width + 4, TREE_ROW - 2),
                    C::Hilight,
                );
            }
            p.text(
                text_x,
                text_y,
                &label,
                Ink::Color(if current {
                    C::HilightText
                } else {
                    C::WindowText
                }),
                false,
            );
        }
    });
    if let Some(bar) = bar {
        p.scrollbar(
            bar,
            true,
            (content - 1, view.height, scroll),
            ui.tree_scroll.pressed(),
        );
    }
}

fn paint_list(p: &mut Painter<'_>, scene: &Scene<'_>, l: &Layout) {
    let Scene { browser, ui, .. } = scene;
    p.edge(l.list, Edge::Sunken);
    p.fill(l.list_inner, C::Window);
    let grid = Grid::new(browser.view, l.list_inner, browser.items.len());
    let scroll = ui.list_scroll.position;
    p.clipped(grid.viewport, |p| {
        for (index, item) in browser.items.iter().enumerate() {
            let cell = grid.cell(index, scroll);
            if cell.bottom() < grid.viewport.y
                || cell.y > grid.viewport.bottom()
                || cell.right() < grid.viewport.x
                || cell.x > grid.viewport.right()
            {
                continue;
            }
            paint_item(p, scene, &grid, item, cell, browser.is_selected(index));
            if let Some(edit) = ui
                .rename
                .as_ref()
                .filter(|(i, _)| *i == index)
                .map(|(_, e)| e)
            {
                let rect = if grid.view == ViewMode::LargeIcons {
                    Rect::new(cell.x + 2, cell.y + 38, cell.width - 4, 17)
                } else {
                    Rect::new(cell.x + 20, cell.y, (cell.width - 22).min(220), cell.height)
                };
                p.edit(rect, edit, true, false);
            }
        }
    });
    if grid.view == ViewMode::Details {
        paint_header(p, &grid, &ui.columns);
    }
    if let Some(bar) = grid.bar {
        p.scrollbar(
            bar,
            grid.vertical(),
            (grid.extent - 1, grid.page(), scroll),
            ui.list_scroll.pressed(),
        );
    }
}

/// "详细信息"表头条：左端对齐列表视口，高度一行；命中与绘制共用。
pub fn header_bar(grid: &Grid, columns: &ColumnHeader) -> Rect {
    Rect::new(
        grid.viewport.x,
        grid.viewport.y,
        columns.total(),
        HEADER_HEIGHT,
    )
}

fn paint_header(p: &mut Painter<'_>, grid: &Grid, columns: &ColumnHeader) {
    let mut x = grid.viewport.x;
    for (title, full) in ["名称", "大小", "类型", "修改时间"]
        .into_iter()
        .zip(columns.cells(header_bar(grid, columns)))
    {
        let cell = Rect::new(
            x,
            grid.viewport.y,
            full.width.min(grid.viewport.right() - x).max(0),
            HEADER_HEIGHT,
        );
        if cell.width <= 0 {
            break;
        }
        let face = p.edge(cell, Edge::Raised);
        p.fill(face, C::ButtonFace);
        p.text_fit(
            cell.x + 6,
            cell.y + (HEADER_HEIGHT - p.line_height()) / 2,
            cell.width - 10,
            title,
            Ink::Color(C::ButtonText),
            false,
        );
        x += full.width;
    }
    let rest = Rect::new(
        x,
        grid.viewport.y,
        (grid.viewport.right() - x).max(0),
        HEADER_HEIGHT,
    );
    if rest.width > 0 {
        let face = p.edge(rest, Edge::Raised);
        p.fill(face, C::ButtonFace);
    }
}

fn paint_item(
    p: &mut Painter<'_>,
    scene: &Scene<'_>,
    grid: &Grid,
    item: &Item,
    cell: Rect,
    selected: bool,
) {
    let assets = scene.assets;
    let focused = scene.active && !scene.ui.address_focus;
    let (background, foreground) = match (selected, focused) {
        (true, true) => (Some(C::Hilight), C::HilightText),
        (true, false) => (Some(C::ButtonFace), C::WindowText),
        _ => (None, C::WindowText),
    };
    let label = |p: &mut Painter<'_>, x: i32, y: i32, text: &str, width: i32| {
        let text = p.fit(text, width);
        let w = logical(p, p.text_width(&text, false));
        if let Some(background) = background {
            p.fill(
                Rect::new(x - 2, y - 1, w + 4, p.line_height() + 2),
                background,
            );
        }
        p.text(x, y, &text, Ink::Color(foreground), false);
        if selected && focused {
            p.focus_rect(Rect::new(x - 2, y - 1, w + 4, p.line_height() + 2));
        }
    };
    if grid.view == ViewMode::LargeIcons {
        draw(
            Assets::icon(&assets.large, item.icon),
            p,
            cell.x + (cell.width - 32) / 2,
            cell.y + 4,
        );
        let shown = p.fit(&item.name, cell.width - 6);
        let width = logical(p, p.text_width(&shown, false));
        label(
            p,
            cell.x + (cell.width - width) / 2,
            cell.y + 40,
            &shown,
            cell.width - 6,
        );
        return;
    }
    draw(
        Assets::icon(&assets.small, item.icon),
        p,
        cell.x + 2,
        cell.y + 1,
    );
    let text_y = cell.y + (cell.height - p.line_height()) / 2;
    if grid.view != ViewMode::Details {
        label(p, cell.x + 22, text_y, &item.name, cell.width - 26);
        return;
    }
    let columns = &scene.ui.columns;
    let (name, size_column, kind_column, date_column) = (
        columns.width(0),
        columns.width(1),
        columns.width(2),
        columns.width(3),
    );
    label(p, cell.x + 22, text_y, &item.name, name - 26);
    let size = item.size.map(size_text).unwrap_or_default();
    let size_width = logical(p, p.text_width(&size, false));
    let modified = item.modified.map(date_text).unwrap_or_default();
    let cells = [
        (
            cell.x + name + size_column - 8 - size_width,
            size,
            size_column,
        ),
        (
            cell.x + name + size_column + 6,
            item.kind.clone(),
            kind_column - 8,
        ),
        (
            cell.x + name + size_column + kind_column + 6,
            modified,
            date_column - 8,
        ),
    ];
    for (x, text, width) in cells {
        p.text(
            x,
            text_y,
            &p.fit(&text, width),
            Ink::Color(C::WindowText),
            false,
        );
    }
}

fn paint_status(p: &mut Painter<'_>, scene: &Scene<'_>, status: Rect) {
    let Scene {
        browser, assets, ..
    } = scene;
    let right = Rect::new(status.right() - 140, status.y + 2, 138, status.height - 3);
    let middle = Rect::new(right.x - 92, status.y + 2, 90, status.height - 3);
    let left = Rect::new(
        status.x + 1,
        status.y + 2,
        (middle.x - 2 - status.x - 1).max(0),
        status.height - 3,
    );
    let selected = browser.selected.and_then(|i| browser.items.get(i));
    let summary = if let Some(message) = &browser.message {
        message.clone()
    } else if browser.truncated {
        format!(
            "{} 个对象（目录过大，只显示前 {}）",
            browser.items.len(),
            crate::model::MAX_ENTRIES
        )
    } else if browser.selection().len() > 1 {
        format!("选定 {} 个对象", browser.selection().len())
    } else if selected.is_some() {
        "选定 1 个对象".to_owned()
    } else {
        format!("{} 个对象", browser.items.len())
    };
    let size = selected
        .and_then(|item| item.size)
        .map(size_text)
        .unwrap_or_default();
    for (rect, text, icon) in [
        (left, summary, false),
        (middle, size, false),
        (right, "我的电脑".to_owned(), true),
    ] {
        p.edge(rect, Edge::SunkenOuter);
        let inner = rect.inset(1);
        p.clipped(inner, |p| {
            let mut x = inner.x + 4;
            if icon {
                draw(Assets::icon(&assets.small, Icon::MyComputer), p, x, inner.y);
                x += 20;
            }
            p.text(
                x,
                inner.y + (inner.height - p.line_height()) / 2,
                &p.fit(&text, inner.right() - x - 2),
                Ink::Color(C::ButtonText),
                false,
            );
        });
    }
}

fn paint_popup(p: &mut Painter<'_>, popup: &Popup) {
    let rect = popup.rect();
    let inner = p.menu_popup(rect);
    let mut y = inner.y;
    for (index, entry) in popup.entries.iter().enumerate() {
        match entry {
            Entry::Separator => y += p.menu_separator(Rect::new(inner.x, y, inner.width, 8)),
            Entry::Item {
                label,
                shortcut,
                command,
                checked,
            } => {
                p.menu_item(
                    Rect::new(inner.x, y, inner.width, ITEM_HEIGHT),
                    label,
                    shortcut,
                    MenuItemState {
                        highlighted: popup.hover == Some(index) && command.is_some(),
                        disabled: command.is_none(),
                        checked: *checked,
                        has_submenu: false,
                    },
                );
                y += ITEM_HEIGHT;
            }
        }
    }
}
