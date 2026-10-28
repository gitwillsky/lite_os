//! “打开方式”对话框；安装条目与持久默认值由 desktop-entry 拥有。
use desktop_entry::{Associations, Entry};
use liteos_ui::{
    Column, Dialogs, EventKind, Layout, RowIcon, Widget, WidgetKind, WidgetSpec, Window,
    WindowOptions, state,
};
use std::path::Path;

fn node(
    parent: &Widget,
    kind: WidgetKind,
    text: &str,
    layout: Layout,
) -> liteos_ui::Result<Widget> {
    let mut spec = WidgetSpec::new(kind, text);
    spec.layout = layout;
    parent.child(spec)
}

/// 选择可接收文件的已安装程序；返回条目与是否设为默认，取消返回 None。
pub async fn choose(
    dialogs: &Dialogs,
    parent: &Window,
    path: &Path,
    associations: &Associations,
) -> liteos_ui::Result<Option<(Entry, bool)>> {
    let mut options = WindowOptions::new("打开方式", "org.liteos.file-explorer", 420, 350);
    options.fixed = true;
    options.modal = true;
    options.parent = Some(parent);
    let (window, mut events) = dialogs.window(options)?;
    let mut spec = WidgetSpec::new(WidgetKind::Column, "");
    spec.layout = Layout {
        grow: 1,
        padding: 12,
        gap: 8,
        ..Default::default()
    };
    let root = window.widget(spec)?;
    let prompt = node(
        &root,
        WidgetKind::Label,
        &format!(
            "请选择用于打开“{}”的程序：",
            path.file_name().unwrap_or_default().to_string_lossy()
        ),
        Layout {
            height: 40,
            ..Default::default()
        },
    )?;
    prompt.set_state(state::WRAP)?;
    let list = node(
        &root,
        WidgetKind::List,
        "",
        Layout {
            grow: 1,
            ..Default::default()
        },
    )?;
    list.set_state(state::CLIENT_EDGE)?;
    list.set_columns(&[Column {
        id: 1,
        title: "程序",
        width: 365,
    }])?;
    let choices = associations.choices(path);
    list.set_rows(
        &choices
            .iter()
            .enumerate()
            .map(|(index, entry)| (index as u64 + 1, entry.name.as_str()))
            .collect::<Vec<_>>(),
    )?;
    list.set_row_icons(
        &choices
            .iter()
            .enumerate()
            .map(|(index, entry)| RowIcon {
                id: index as u64 + 1,
                source: entry.icon.as_deref().map_or(
                    liteos_ui::Icon::Resource("file-app"),
                    liteos_ui::Icon::Theme,
                ),
            })
            .collect::<Vec<_>>(),
    )?;
    let selected = associations
        .default_for(path)
        .and_then(|default| choices.iter().position(|entry| entry.id() == default.id()))
        .unwrap_or(0);
    if !choices.is_empty() {
        list.select(Some(selected as u64 + 1))?;
    }
    let always = node(
        &root,
        WidgetKind::CheckBox,
        "始终使用此程序打开这类文件(&A)",
        Layout {
            height: 23,
            ..Default::default()
        },
    )?;
    if associations.mime_for(path).is_none() {
        always.set_state(state::DISABLED)?;
    }
    let buttons = node(
        &root,
        WidgetKind::Row,
        "",
        Layout {
            height: 23,
            gap: 6,
            ..Default::default()
        },
    )?;
    let spacer = node(
        &buttons,
        WidgetKind::Label,
        "",
        Layout {
            grow: 1,
            ..Default::default()
        },
    )?;
    let ok = node(
        &buttons,
        WidgetKind::Button,
        "确定",
        Layout {
            width: 75,
            ..Default::default()
        },
    )?;
    ok.set_state(if choices.is_empty() {
        state::DISABLED
    } else {
        state::DEFAULT
    })?;
    let cancel = node(
        &buttons,
        WidgetKind::Button,
        "取消",
        Layout {
            width: 75,
            ..Default::default()
        },
    )?;
    list.focus()?;
    let _held = [&prompt, &spacer];
    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => return Ok(None),
            EventKind::Activate { .. } if event.node == cancel.id() => return Ok(None),
            EventKind::Activate { .. } if event.node == ok.id() || event.node == list.id() => {
                if let Some(entry) = list
                    .selection()?
                    .and_then(|id| id.checked_sub(1))
                    .and_then(|index| choices.get(index as usize))
                {
                    return Ok(Some((entry.clone(), always.state()? & state::CHECKED != 0)));
                }
            }
            _ => {}
        }
    }
    Ok(None)
}
