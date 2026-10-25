//! 用 SDK 声明窗口、标准控件与业务数据；不拥有渲染、输入、展开或选择状态。
use crate::{
    browser::Browser,
    chrome::{self, Availability, Command},
};
use liteos_ui::{
    Column, ColumnAlignment, Layout, Menu, RowIcon, TreeRow, Widget, WidgetKind as Kind,
    WidgetSpec, Window, state,
};
pub struct View {
    pub window: Window,
    pub menu: Widget,
    pub toolbar: Widget,
    pub tools: Vec<(Command, Widget)>,
    pub address: Widget,
    pub go: Widget,
    pub list: Widget,
    pub folders: Widget,
    pub status: Widget,
    _labels: Vec<Widget>,
}
fn child(parent: &Widget, kind: Kind, text: &str, layout: Layout) -> liteos_ui::Result<Widget> {
    let mut spec = WidgetSpec::new(kind, text);
    spec.layout = layout;
    parent.child(spec)
}
fn clean(text: &str) -> String {
    text.replace(['\t', '\n', '\r'], " ")
}
impl View {
    pub fn new(window: Window) -> liteos_ui::Result<Self> {
        let mut spec = WidgetSpec::new(Kind::Column, "");
        spec.layout.grow = 1;
        let root = window.widget(spec)?;
        let menu = child(&root, Kind::MenuBar, "", Layout::default())?;
        let toolbar = child(
            &root,
            Kind::Row,
            "",
            Layout {
                height: 30,
                gap: 4,
                padding: 2,
                ..Default::default()
            },
        )?;
        let mut tools = Vec::new();
        for (command, title) in [
            (Command::Back, "后退"),
            (Command::Forward, "前进"),
            (Command::Up, "向上"),
            (Command::ToggleFolders, "文件夹"),
            (Command::Copy, "复制"),
            (Command::Paste, "粘贴"),
            (Command::Properties, "属性"),
            (Command::ViewMenu, "查看"),
        ] {
            tools.push((
                command,
                child(
                    &toolbar,
                    Kind::Button,
                    title,
                    Layout {
                        width: 60,
                        height: 25,
                        ..Default::default()
                    },
                )?,
            ));
        }
        let bar = child(
            &root,
            Kind::Row,
            "",
            Layout {
                height: 28,
                gap: 6,
                padding: 2,
                ..Default::default()
            },
        )?;
        let title = child(
            &bar,
            Kind::Label,
            "地址(D)",
            Layout {
                width: 60,
                ..Default::default()
            },
        )?;
        let address = child(
            &bar,
            Kind::Edit,
            "",
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        let go = child(
            &bar,
            Kind::Button,
            "转到",
            Layout {
                width: 60,
                ..Default::default()
            },
        )?;
        let body = child(
            &root,
            Kind::Row,
            "",
            Layout {
                grow: 1,
                gap: 3,
                ..Default::default()
            },
        )?;
        let folders = child(
            &body,
            Kind::Tree,
            "",
            Layout {
                width: 200,
                min_width: 120,
                ..Default::default()
            },
        )?;
        folders.set_state(state::HIDDEN)?;
        let list = child(
            &body,
            Kind::List,
            "",
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        list.set_list_style(liteos_ui::ListStyle::Details, true)?;
        let status = child(&root, Kind::Status, "正在读取…", Layout::default())?;
        Ok(Self {
            window,
            menu,
            toolbar,
            tools,
            address,
            go,
            list,
            folders,
            status,
            _labels: vec![root, bar, title, body],
        })
    }
    pub fn rows(&self, browser: &Browser) -> liteos_ui::Result<()> {
        let (column, descending) = browser.sort;
        self.list.set_columns(
            &[
                (1, "名称", 200),
                (2, "大小", 100),
                (3, "类型", 145),
                (4, "修改时间", 160),
            ]
            .into_iter()
            .map(|(id, title, width)| Column {
                id,
                title,
                width,
                sort: if column == id {
                    if descending { 2 } else { 1 }
                } else {
                    0
                },
            })
            .collect::<Vec<_>>(),
        )?;
        self.list.set_column_alignment(2, ColumnAlignment::Right)?;
        let rows: Vec<_> = browser
            .items
            .iter()
            .map(|(id, item)| {
                (
                    *id,
                    format!(
                        "{}\t{}\t{}\t{}",
                        clean(&item.name),
                        item.size.map_or_else(String::new, crate::model::size_text),
                        clean(&item.kind),
                        item.modified
                            .map_or_else(String::new, crate::model::date_text)
                    ),
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
    pub fn tree(&self, browser: &Browser) -> liteos_ui::Result<()> {
        let current = self.folders.selection()?.filter(|id| {
            browser
                .catalogue
                .nodes
                .get(id)
                .is_some_and(|node| node.location == browser.location)
        });
        let rows: Vec<_> = browser
            .catalogue
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
        self.folders.set_row_icons(
            &browser
                .catalogue
                .nodes
                .iter()
                .map(|(id, node)| RowIcon {
                    id: *id,
                    source: liteos_ui::Icon::Resource(node.icon.file_stem()),
                })
                .collect::<Vec<_>>(),
        )?;
        self.folders
            .select(current.or_else(|| browser.catalogue.id(&browser.location)))
    }
    pub fn menus(&self, browser: &Browser, state: &Availability) -> liteos_ui::Result<()> {
        let menus = chrome::menus(state);
        self.menu.set_menus(
            &menus
                .iter()
                .map(|(title, items)| Menu { title, items })
                .collect::<Vec<_>>(),
        )?;
        for (command, button) in &self.tools {
            let enabled = !state.busy
                && match command {
                    Command::Back => browser.history(-1).is_some(),
                    Command::Forward => browser.history(1).is_some(),
                    Command::Up => browser.location.parent().is_some(),
                    Command::Copy | Command::Properties => state.selected,
                    Command::Paste => state.editable && state.paste,
                    _ => true,
                };
            button.set_state(if enabled { 0 } else { state::DISABLED })?;
        }
        Ok(())
    }
}
