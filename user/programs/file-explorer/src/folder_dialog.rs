//! “浏览文件夹”对话框（shell32 `SHBrowseForFolder`，`BIF_RETURNONLYFSDIRS`）：提示文字、外壳命名空间树、确定/取消。
//! 树复用资源管理器的目录与分支读取；只有真实目录可被选中返回。
use crate::{
    browser::{self, Catalogue},
    model::Location,
};
use liteos_ui::{
    Background, Dialogs, EventKind, Layout, RowIcon, TreeRow, Widget, WidgetKind as Kind,
    WidgetSpec, Window, WindowOptions, state,
};
use std::path::{Path, PathBuf};

fn child(parent: &Widget, kind: Kind, text: &str, layout: Layout) -> liteos_ui::Result<Widget> {
    let mut spec = WidgetSpec::new(kind, text);
    spec.layout = layout;
    parent.child(spec)
}

fn show(tree: &Widget, catalogue: &Catalogue) -> liteos_ui::Result<()> {
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
    tree.set_tree_rows(&rows)?;
    tree.set_row_icons(
        &catalogue
            .nodes
            .iter()
            .map(|(id, node)| RowIcon {
                id: *id,
                source: liteos_ui::Icon::Resource(node.icon.file_stem()),
            })
            .collect::<Vec<_>>(),
    )
}

/// 打开对话框并等待结果；取消、Escape 或关闭返回 None。`start` 所在的目录链预先展开并选中。
pub async fn browse(
    dialogs: &Dialogs,
    background: &Background,
    parent: &Window,
    title: &str,
    prompt: &str,
    start: &Path,
) -> Result<Option<PathBuf>, Box<dyn std::error::Error>> {
    let mut options = WindowOptions::new(title, "org.liteos.file-explorer", 320, 330);
    options.fixed = true;
    options.modal = true;
    options.parent = Some(parent);
    let (window, mut events) = dialogs.window(options)?;
    let mut spec = WidgetSpec::new(Kind::Column, "");
    spec.layout = Layout {
        grow: 1,
        padding: 7,
        gap: 7,
        ..Default::default()
    };
    let root = window.widget(spec)?;
    let label = child(&root, Kind::Label, prompt, Layout::default())?;
    label.set_state(state::WRAP)?;
    let tree = child(
        &root,
        Kind::Tree,
        "",
        Layout {
            grow: 1,
            ..Default::default()
        },
    )?;
    let buttons = child(&root, Kind::Row, "", Layout::default())?;
    let filler = child(
        &buttons,
        Kind::Column,
        "",
        Layout {
            grow: 1,
            ..Default::default()
        },
    )?;
    let size = Layout {
        width: 75,
        height: 23,
        ..Default::default()
    };
    let ok = child(&buttons, Kind::Button, "确定", size)?;
    ok.set_state(state::DEFAULT)?;
    let cancel = child(&buttons, Kind::Button, "取消", size)?;

    // 预读起点的祖先链，使初始选择可见（与 BFFM_SETSELECTION 相同）。
    let mut catalogue = Catalogue::new();
    let mut chain = vec![Location::Directory(start.to_owned())];
    while let Some(parent) = chain.last().and_then(Location::parent) {
        chain.push(parent);
    }
    for location in chain.iter().rev() {
        let location = location.clone();
        if let Ok(branch) = background
            .run(move || browser::read_branch(location))?
            .await
        {
            catalogue.update(branch);
        }
    }
    show(&tree, &catalogue)?;
    let initial = catalogue.id(&chain[0]);
    tree.select(initial)?;
    tree.focus()?;
    let chosen = |catalogue: &Catalogue, id: Option<u64>| match id
        .and_then(|id| catalogue.nodes.get(&id))
        .map(|node| &node.location)
    {
        Some(Location::Directory(path)) if !Location::Directory(path.clone()).is_recycle_bin() => {
            Some(path.clone())
        }
        _ => None,
    };
    ok.set_state(if chosen(&catalogue, initial).is_some() {
        state::DEFAULT
    } else {
        state::DEFAULT | state::DISABLED
    })?;
    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close => return Ok(None),
            EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => return Ok(None),
            EventKind::Activate { .. } if event.node == cancel.id() => return Ok(None),
            EventKind::Activate { .. } if event.node == ok.id() => {
                if let Some(path) = chosen(&catalogue, tree.selection()?) {
                    return Ok(Some(path));
                }
            }
            EventKind::Select { row } if event.node == tree.id() => {
                // 只有真实目录可以返回；虚拟文件夹与回收站选中时“确定”不可用。
                ok.set_state(if chosen(&catalogue, Some(row)).is_some() {
                    state::DEFAULT
                } else {
                    state::DEFAULT | state::DISABLED
                })?;
            }
            EventKind::Expand {
                row,
                expanded: true,
            } if event.node == tree.id() => {
                if let Some(location) = catalogue.nodes.get(&row).map(|node| node.location.clone())
                {
                    let branch = background
                        .run(move || browser::read_branch(location))?
                        .await?;
                    catalogue.update(branch);
                    show(&tree, &catalogue)?;
                }
            }
            _ => {}
        }
    }
    drop((label, buttons, filler));
    Ok(None)
}
