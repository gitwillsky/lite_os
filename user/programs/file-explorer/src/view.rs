//! 用 SDK 声明窗口、标准控件与业务数据；不拥有渲染、输入、展开或选择状态。
//!
//! 窗口结构按 Windows 2000 资源管理器：ReBar（菜单栏、标准按钮、地址栏三行带区与品牌 logo）、
//! 可选的“文件夹”浏览栏、Web 视图面板加列表视图、三格状态栏（文字、大小、区域）。
use crate::{
    browser::Browser,
    chrome::{self, Action, Availability, Command},
    model::Location,
    webview::{self, Part},
};
use liteos_ui::{
    Column, ColumnAlignment, Draw, FontRole, Layout, Menu, Rect, RowIcon, TreeRow, Widget,
    WidgetKind as Kind, WidgetSpec, Window, state,
};
use std::{cell::RefCell, path::Path};

/// WEBVIEW.CSS `#Panel {width: 200px}`；客户区宽度小于它的两倍时隐藏面板（`#FileList` 的 left 表达式）。
const PANEL_WIDTH: i32 = 200;
/// 状态栏分格：文字格占满剩余，大小格与区域格固定宽（browseui 的 STATUS_PANE_SIZE / STATUS_PANE_ZONE）。
const STATUS_PARTS: [i32; 3] = [-1, 90, 150];
/// 区域格（Internet 安全区域）的文字与图标：本机文件夹都属于“我的电脑”区域。
const ZONE: &str = "我的电脑";

/// Web 视图中可激活的控件对应的去处。
pub enum Target {
    Place(Location),
    Do(Action),
}

pub struct View {
    pub window: Window,
    pub rebar: Widget,
    pub menu: Widget,
    pub toolbar: Widget,
    pub address_band: Widget,
    pub address: Widget,
    pub go: Widget,
    pub folders_pane: Widget,
    pub folders_close: Widget,
    pub folders: Widget,
    content: Widget,
    panel: Widget,
    panel_icon: Widget,
    panel_title: Widget,
    details: Widget,
    /// Web 视图正文的控件与激活去处；每次刷新整体替换，旧控件析构即从窗口移除。
    parts: RefCell<Vec<(Widget, Option<Target>)>>,
    pub list: Widget,
    pub status: Widget,
    _layout: Vec<Widget>,
}

fn child(parent: &Widget, kind: Kind, text: &str, layout: Layout) -> liteos_ui::Result<Widget> {
    let mut spec = WidgetSpec::new(kind, text);
    spec.layout = layout;
    parent.child(spec)
}

/// 固定尺寸的空白（CSS 的 margin/padding）。
fn spacer(parent: &Widget, width: i32, height: i32) -> liteos_ui::Result<Widget> {
    child(
        parent,
        Kind::Column,
        "",
        Layout {
            width,
            height,
            gap: 0,
            ..Default::default()
        },
    )
}

fn stack(parent: &Widget, kind: Kind, layout: Layout) -> liteos_ui::Result<Widget> {
    child(parent, kind, "", Layout { gap: 0, ..layout })
}

fn clean(text: &str) -> String {
    text.replace(['\t', '\n', '\r'], " ")
}

impl View {
    pub fn new(window: Window) -> liteos_ui::Result<Self> {
        let mut spec = WidgetSpec::new(Kind::Column, "");
        spec.layout.grow = 1;
        spec.layout.gap = 0;
        let root = window.widget(spec)?;
        let rebar = stack(&root, Kind::Rebar, Layout::default())?;
        let menu = child(&rebar, Kind::MenuBar, "", Layout::default())?;
        let toolbar = child(&rebar, Kind::Toolbar, "", Layout::default())?;
        let address_band = child(
            &rebar,
            Kind::Row,
            "",
            Layout {
                gap: 4,
                ..Default::default()
            },
        )?;
        let address_label = child(&address_band, Kind::Label, "地址(D)", Layout::default())?;
        let address = child(
            &address_band,
            Kind::Combo,
            "",
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        let go = child(&address_band, Kind::Toolbar, "", Layout::default())?;
        go.set_toolbar(&[liteos_ui::ToolbarButton {
            id: Action::Address as u64,
            kind: liteos_ui::ToolbarKind::Button,
            strip: "go",
            frame: 0,
            text: "转到",
            show_text: true,
            enabled: true,
            checked: false,
        }])?;

        let body = stack(
            &root,
            Kind::Row,
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        // “文件夹”浏览栏：标题栏（文字与关闭按钮）加文件夹树。
        let folders_pane = stack(
            &body,
            Kind::Column,
            Layout {
                width: 200,
                min_width: 120,
                ..Default::default()
            },
        )?;
        folders_pane.set_state(state::HIDDEN)?;
        let caption = child(
            &folders_pane,
            Kind::Row,
            "",
            Layout {
                padding: 2,
                gap: 4,
                ..Default::default()
            },
        )?;
        let caption_text = child(
            &caption,
            Kind::Label,
            "文件夹",
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        let folders_close = child(
            &caption,
            Kind::Button,
            "×",
            Layout {
                width: 16,
                height: 14,
                ..Default::default()
            },
        )?;
        let folders = child(
            &folders_pane,
            Kind::Tree,
            "",
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        let separator = spacer(&body, 3, -1)?;

        let content = stack(
            &body,
            Kind::Row,
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        // Web 视图面板：白底，左上角 wvleft.bmp 水印（WEBVIEW.CSS `#Panel`）。
        let panel = stack(
            &content,
            Kind::Column,
            Layout {
                width: PANEL_WIDTH,
                ..Default::default()
            },
        )?;
        panel.set_background(Some(0xFF_FFFF), Some("webview-left-1x"))?;
        // #Corner {padding-left: 12px; padding-top: 11px}；#FolderName {margin-top: 8px}。
        let top = spacer(&panel, -1, 11)?;
        let corner = stack(&panel, Kind::Row, Layout::default())?;
        let corner_left = spacer(&corner, 12, -1)?;
        let corner_body = child(
            &corner,
            Kind::Column,
            "",
            Layout {
                grow: 1,
                gap: 8,
                ..Default::default()
            },
        )?;
        let panel_icon = child(
            &corner_body,
            Kind::Image,
            "folder-closed",
            Layout {
                width: 32,
                height: 32,
                ..Default::default()
            },
        )?;
        let panel_title = child(&corner_body, Kind::Label, "", Layout::default())?;
        panel_title.set_state(state::WRAP)?;
        panel_title.set_font(FontRole::Menu, Some(14), Some(700))?;
        // #LogoLine {width: 100%; height: 2px; margin-top: 4px}。
        let line_gap = spacer(&panel, -1, 4)?;
        let line = child(
            &panel,
            Kind::Image,
            "webview-line-1x",
            Layout {
                height: 2,
                ..Default::default()
            },
        )?;
        // #Details {padding-left: 12px; margin-top: 8px}。
        let details_gap = spacer(&panel, -1, 8)?;
        let details_row = stack(
            &panel,
            Kind::Row,
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        let details_left = spacer(&details_row, 12, -1)?;
        let details = stack(
            &details_row,
            Kind::Column,
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        let list = child(
            &content,
            Kind::List,
            "",
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        list.set_list_style(liteos_ui::ListStyle::Details, true)?;
        let status = child(&root, Kind::Status, "\t\t", Layout::default())?;
        status.set_status_parts(&STATUS_PARTS)?;
        Ok(Self {
            window,
            rebar,
            menu,
            toolbar,
            address_band,
            address,
            go,
            folders_pane,
            folders_close,
            folders,
            content,
            panel,
            panel_icon,
            panel_title,
            details,
            parts: RefCell::new(Vec::new()),
            list,
            status,
            _layout: vec![
                root,
                address_label,
                body,
                caption,
                caption_text,
                separator,
                top,
                corner,
                corner_left,
                corner_body,
                line_gap,
                line,
                details_gap,
                details_row,
                details_left,
            ],
        })
    }

    /// 按客户区宽度显示或隐藏 Web 视图面板。
    pub fn fit_panel(&self) -> liteos_ui::Result<()> {
        let width = self.content.geometry()?.width;
        let hidden = width < PANEL_WIDTH * 2;
        let current = self.panel.state()? & state::HIDDEN != 0;
        if hidden != current {
            self.panel
                .set_state(if hidden { state::HIDDEN } else { 0 })?;
        }
        Ok(())
    }

    pub fn rows(&self, browser: &Browser) -> liteos_ui::Result<()> {
        let (column, descending) = browser.sort;
        let columns = browser.location.columns();
        self.list.set_columns(
            &columns
                .iter()
                .map(|(field, title, width)| Column {
                    id: *field as u64,
                    title,
                    width: *width,
                    sort: if column == *field {
                        if descending { 2 } else { 1 }
                    } else {
                        0
                    },
                })
                .collect::<Vec<_>>(),
        )?;
        for (field, ..) in columns.iter().filter(|(field, ..)| field.right_aligned()) {
            self.list
                .set_column_alignment(*field as u64, ColumnAlignment::Right)?;
        }
        let rows: Vec<_> = browser
            .items
            .iter()
            .map(|(id, item)| {
                (
                    *id,
                    columns
                        .iter()
                        .map(|(field, ..)| clean(&item.cell(*field)))
                        .collect::<Vec<_>>()
                        .join("\t"),
                )
            })
            .collect();
        self.list.set_rows(
            &rows
                .iter()
                .map(|(id, text)| (*id, text.as_str()))
                .collect::<Vec<_>>(),
        )?;
        self.list.set_row_icons(
            &browser
                .items
                .iter()
                .map(|(id, item)| RowIcon {
                    id: *id,
                    source: liteos_ui::Icon::Resource(item.icon.file_stem()),
                })
                .collect::<Vec<_>>(),
        )?;
        Ok(())
    }

    /// 文件夹树与地址下拉共用同一份目录：树显示已加载的全部分支，地址下拉只显示当前位置的祖先链
    /// 与根下的系统文件夹（ComboBoxEx 的 Windows 2000 内容）。
    pub fn tree(&self, browser: &Browser) -> liteos_ui::Result<()> {
        let catalogue = &browser.catalogue;
        let current = self.folders.selection()?.filter(|id| {
            catalogue
                .nodes
                .get(id)
                .is_some_and(|node| node.location == browser.location)
        });
        let rows: Vec<_> = catalogue
            .nodes
            .iter()
            .map(|(id, node)| TreeRow {
                id: *id,
                parent: node.parent,
                text: &node.name,
                expanded: node.children.is_some(),
                has_children: node
                    .children
                    .as_ref()
                    .is_none_or(|children| !children.is_empty()),
            })
            .collect();
        self.folders.set_tree_rows(&rows)?;
        let icons: Vec<_> = catalogue
            .nodes
            .iter()
            .map(|(id, node)| RowIcon {
                id: *id,
                source: liteos_ui::Icon::Resource(node.icon.file_stem()),
            })
            .collect();
        self.folders.set_row_icons(&icons)?;
        let selected = current.or_else(|| catalogue.id(&browser.location));
        self.folders.select(selected)?;

        // 桌面、桌面下的各项、我的电脑下的各驱动器与控制面板，加上当前位置的祖先链。
        let chain = selected
            .map(|id| catalogue.ancestors(id))
            .unwrap_or_default();
        let parent_of = |id: u64| {
            catalogue
                .nodes
                .get(&id)
                .map(|node| (node.parent, &node.location))
        };
        let shown: Vec<_> = catalogue
            .nodes
            .iter()
            .filter(|(id, node)| {
                node.parent == 0
                    || chain.contains(id)
                    || parent_of(node.parent).is_some_and(|(grandparent, location)| {
                        grandparent == 0 || *location == Location::MyComputer
                    })
            })
            .map(|(id, node)| TreeRow {
                id: *id,
                parent: node.parent,
                text: &node.name,
                expanded: true,
                has_children: false,
            })
            .collect();
        self.address.set_tree_rows(&shown)?;
        let shown_ids: std::collections::BTreeSet<_> = shown.iter().map(|row| row.id).collect();
        self.address.set_row_icons(
            &icons
                .into_iter()
                .filter(|icon| shown_ids.contains(&icon.id))
                .collect::<Vec<_>>(),
        )?;
        self.address
            .select(selected.filter(|id| shown_ids.contains(id)))?;
        self.address.set_text(&browser.location.address())
    }

    /// 菜单栏、标准按钮与“文件夹”按钮状态。
    pub fn menus(&self, state: &Availability) -> liteos_ui::Result<()> {
        let menus = chrome::menus(state);
        let items: Vec<Vec<_>> = menus
            .iter()
            .map(|(_, entries)| entries.iter().map(chrome::Entry::item).collect())
            .collect();
        self.menu.set_menus(
            &menus
                .iter()
                .zip(&items)
                .map(|((title, _), items)| Menu { title, items })
                .collect::<Vec<_>>(),
        )?;
        self.toolbar.set_toolbar(&chrome::standard_buttons(state))
    }

    /// 状态栏：对象数（shell32 6466/6468/6477）、选中或全部文件的大小、区域格。
    pub fn status(
        &self,
        browser: &Browser,
        selected: &[u64],
        free: Option<u64>,
    ) -> liteos_ui::Result<()> {
        let items: Vec<_> = if selected.is_empty() {
            browser.items.iter().map(|(_, item)| item).collect()
        } else {
            selected.iter().filter_map(|id| browser.item(*id)).collect()
        };
        let text = if browser.truncated {
            format!(
                "{} 个对象（目录过大，仅显示前 4096 项）",
                browser.items.len()
            )
        } else if !selected.is_empty() {
            format!("选定 {} 个对象", selected.len())
        } else if let Some(free) = free {
            format!(
                "{} 个对象(可用磁盘空间: {})",
                browser.items.len(),
                crate::model::byte_size_text(free)
            )
        } else {
            format!("{} 个对象", browser.items.len())
        };
        let sizes: Vec<u64> = items.iter().filter_map(|item| item.size).collect();
        let size = if sizes.is_empty() {
            String::new()
        } else {
            crate::model::byte_size_text(sizes.iter().sum())
        };
        self.status.set_text(&format!("{text}\t{size}\t{ZONE}"))?;
        self.status.set_row_icons(&[RowIcon {
            id: 3,
            source: liteos_ui::Icon::Resource("my-computer"),
        }])
    }

    /// 刷新 Web 视图：标题、图标与正文。
    pub fn web_view(
        &self,
        browser: &Browser,
        home: &Path,
        selected: &[u64],
    ) -> liteos_ui::Result<()> {
        self.panel_title
            .set_text(&webview::title(&browser.location))?;
        self.panel_icon
            .set_text(browser.location.icon().file_stem())?;
        let items: Vec<_> = browser.items.iter().map(|(_, item)| item).collect();
        let chosen: Vec<_> = selected.iter().filter_map(|id| browser.item(*id)).collect();
        let parts = webview::parts(&browser.location, home, &items, &chosen);
        let mut widgets = Vec::new();
        for part in parts {
            self.part(part, &mut widgets)?;
        }
        *self.parts.borrow_mut() = widgets;
        Ok(())
    }

    fn part(&self, part: Part, out: &mut Vec<(Widget, Option<Target>)>) -> liteos_ui::Result<()> {
        let margin = match &part {
            Part::Text { margin, .. }
            | Part::Bold { margin, .. }
            | Part::Link { margin, .. }
            | Part::Button { margin, .. } => *margin,
            Part::Drive { .. } => 12,
        };
        if margin > 0 {
            out.push((spacer(&self.details, -1, margin)?, None));
        }
        match part {
            Part::Text { text, .. } | Part::Bold { text, .. } if text.is_empty() => {}
            Part::Text { text, .. } => {
                let label = child(&self.details, Kind::Label, &text, Layout::default())?;
                label.set_state(state::WRAP)?;
                label.set_font(FontRole::Menu, None, None)?;
                out.push((label, None));
            }
            Part::Bold { text, .. } => {
                let label = child(&self.details, Kind::Label, &text, Layout::default())?;
                label.set_state(state::WRAP)?;
                label.set_font(FontRole::Menu, None, Some(700))?;
                out.push((label, None));
            }
            Part::Link { text, target, .. } => {
                let row = stack(&self.details, Kind::Row, Layout::default())?;
                let link = child(&row, Kind::Link, text, Layout::default())?;
                link.set_font(FontRole::Menu, None, None)?;
                out.push((row, None));
                out.push((link, Some(Target::Place(target))));
            }
            Part::Button { text, action, .. } => {
                let row = stack(&self.details, Kind::Row, Layout::default())?;
                // CSS `button {margin-left: 12px}`。
                let gap = spacer(&row, 12, -1)?;
                let button = child(&row, Kind::Button, text, Layout::default())?;
                out.push((row, None));
                out.push((gap, None));
                out.push((button, Some(Target::Do(action))));
            }
            Part::Drive { total, free } => self.drive(total, free, out)?,
        }
        Ok(())
    }

    /// 磁盘容量：总计、带色块的已用/可用图例（webvw 5021–5023）与三维饼图。
    fn drive(
        &self,
        total: u64,
        free: u64,
        out: &mut Vec<(Widget, Option<Target>)>,
    ) -> liteos_ui::Result<()> {
        let size = crate::model::byte_size_text;
        let label = child(
            &self.details,
            Kind::Label,
            &format!("总计: {}", size(total)),
            Layout::default(),
        )?;
        label.set_font(FontRole::Menu, None, None)?;
        out.push((label, None));
        for (color, text) in [
            (
                webview::USED,
                format!("已用空间: {}", size(total.saturating_sub(free))),
            ),
            (webview::FREE, format!("可用空间: {}", size(free))),
        ] {
            let row = child(
                &self.details,
                Kind::Row,
                "",
                Layout {
                    gap: 4,
                    ..Default::default()
                },
            )?;
            let swatch = child(
                &row,
                Kind::Canvas,
                "",
                Layout {
                    width: 10,
                    height: 10,
                    ..Default::default()
                },
            )?;
            swatch.set_drawing(&[
                Draw {
                    kind: 1,
                    rect: Rect::new(0, 0, 10, 10),
                    color,
                    bold: false,
                    text: "",
                },
                Draw {
                    kind: 2,
                    rect: Rect::new(0, 0, 10, 10),
                    color: 0,
                    bold: false,
                    text: "",
                },
            ])?;
            let legend = child(&row, Kind::Label, &text, Layout::default())?;
            legend.set_font(FontRole::Menu, None, None)?;
            out.extend([(row, None), (swatch, None), (legend, None)]);
        }
        out.push((spacer(&self.details, -1, 8)?, None));
        let pie = child(
            &self.details,
            Kind::Canvas,
            "",
            Layout {
                width: webview::PIE_WIDTH,
                height: webview::PIE_HEIGHT,
                ..Default::default()
            },
        )?;
        let commands: Vec<_> = webview::pie(total, free)
            .into_iter()
            .map(|(x, y, width, height, color)| Draw {
                kind: 1,
                rect: Rect::new(x, y, width, height),
                color,
                bold: false,
                text: "",
            })
            .collect();
        pie.set_drawing(&commands)?;
        out.push((pie, None));
        Ok(())
    }

    /// Web 视图中被激活控件的去处。
    pub fn target(&self, node: u64) -> Option<Target> {
        self.parts
            .borrow()
            .iter()
            .find(|(widget, target)| widget.id() == node && target.is_some())
            .and_then(|(_, target)| match target {
                Some(Target::Place(location)) => Some(Target::Place(location.clone())),
                Some(Target::Do(action)) => Some(Target::Do(*action)),
                None => None,
            })
    }

    /// 后退/前进下拉菜单与“查看”菜单在按钮下方打开；返回选中的命令。
    pub fn command_of(&self, node: u64, value: u64) -> Option<Command> {
        if node == self.menu.id() || node == self.toolbar.id() || node == self.go.id() {
            Command::from_id(value)
        } else {
            None
        }
    }
}
