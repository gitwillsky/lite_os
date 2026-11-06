//! shell32 的“选择列”对话框。只有确认后才交出可保存的列布局。
use crate::model::Location;
use desktop_settings::FolderColumn;
use liteos_ui::{
    Dialogs, EventKind, Layout, MessageButtons, Widget, WidgetKind, WidgetSpec, Window,
    WindowOptions, state,
};

fn child(
    parent: &Widget,
    kind: WidgetKind,
    text: &str,
    layout: Layout,
) -> liteos_ui::Result<Widget> {
    let mut spec = WidgetSpec::new(kind, text);
    spec.layout = layout;
    parent.child(spec)
}

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    location: &Location,
    mut columns: Vec<FolderColumn>,
) -> liteos_ui::Result<Option<Vec<FolderColumn>>> {
    use WidgetKind::{Button, Column, Edit, Label, List, Row};
    const TITLE: &str = "选择列";
    let mut options = WindowOptions::new(TITLE, "org.liteos.file-explorer", 410, 320);
    options.parent = Some(parent);
    options.modal = true;
    options.fixed = true;
    let (window, mut events) = dialogs.window(options)?;
    let grow = Layout {
        grow: 1,
        ..Default::default()
    };
    let normal = Layout::default();
    let mut root = WidgetSpec::new(Column, "");
    root.layout = Layout {
        grow: 1,
        padding: 10,
        gap: 8,
        ..normal
    };
    let root = window.widget(root)?;
    let prompt = child(
        &root,
        Label,
        "请复选那些希望在该文件夹中可见的列。使用“上移”或“下移”按钮重新制定列的顺序。",
        Layout {
            height: 36,
            ..normal
        },
    )?;
    prompt.set_state(state::WRAP)?;
    let row = child(&root, Row, "", grow)?;
    let list = child(&row, List, "", grow)?;
    let actions = child(&row, Column, "", normal)?;
    let button = Layout {
        width: 76,
        height: 23,
        ..normal
    };
    let up = child(&actions, Button, "上移(U)", button)?;
    let down = child(&actions, Button, "下移(D)", button)?;
    let show = child(&actions, Button, "显示(S)", button)?;
    let hide = child(&actions, Button, "隐藏(H)", button)?;
    let line = child(&root, Row, "", normal)?;
    let _width_label = child(&line, Label, "选定列的宽度应为", normal)?;
    let width = child(
        &line,
        Edit,
        "",
        Layout {
            width: 60,
            ..normal
        },
    )?;
    let _pixels = child(&line, Label, "像素(W)", normal)?;
    let buttons = child(&root, Row, "", normal)?;
    let _spacer = child(&buttons, Column, "", grow)?;
    let ok = child(&buttons, Button, "确定", button)?;
    ok.set_state(state::DEFAULT)?;
    let cancel = child(&buttons, Button, "取消", button)?;
    let sync = |columns: &[FolderColumn], selected: usize| -> liteos_ui::Result<()> {
        let rows: Vec<_> = columns
            .iter()
            .map(|column| {
                let (_, title) = location.column_title(column.id).expect("known column");
                (column.id, title)
            })
            .collect();
        list.set_rows(&rows)?;
        list.set_row_checks(
            &columns
                .iter()
                .enumerate()
                .map(|(index, column)| (column.id, column.visible, index != 0))
                .collect::<Vec<_>>(),
        )?;
        list.select(Some(columns[selected].id))?;
        width.set_text(&columns[selected].width.to_string())?;
        for (control, enabled) in [
            (&up, selected > 1),
            (&down, selected != 0 && selected + 1 < columns.len()),
            (&show, !columns[selected].visible),
            (&hide, selected != 0 && columns[selected].visible),
        ] {
            control.set_state(if enabled { 0 } else { state::DISABLED })?;
        }
        Ok(())
    };
    let read_width = || -> liteos_ui::Result<Option<i32>> {
        Ok(width
            .text()?
            .trim()
            .parse()
            .ok()
            .filter(|value| (24..=32768).contains(value)))
    };
    let mut selected = 0;
    sync(&columns, selected)?;
    list.focus()?;
    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => break,
            EventKind::Select { row } if event.node == list.id() => {
                if let Some(index) = columns.iter().position(|column| column.id == row)
                    && index != selected
                {
                    if let Some(value) = read_width()? {
                        columns[selected].width = value;
                    }
                    selected = index;
                    sync(&columns, selected)?;
                }
            }
            EventKind::ToggleRowCheck { row } if event.node == list.id() => {
                if let Some(index) = columns.iter().position(|column| column.id == row)
                    && index != 0
                {
                    if let Some(value) = read_width()? {
                        columns[selected].width = value;
                    }
                    columns[index].visible = !columns[index].visible;
                    sync(&columns, selected)?;
                }
            }
            EventKind::Activate { .. } if event.node == cancel.id() => break,
            EventKind::Activate { .. } if event.node == ok.id() => {
                if let Some(value) = read_width()? {
                    columns[selected].width = value;
                    return Ok(Some(columns));
                }
                let _ = dialogs
                    .message(
                        &window,
                        TITLE,
                        "请输入 24 到 32768 之间的列宽。",
                        MessageButtons::Ok,
                    )?
                    .await?;
                width.focus()?;
            }
            EventKind::Activate { .. }
                if [up.id(), down.id(), show.id(), hide.id()].contains(&event.node) =>
            {
                if let Some(value) = read_width()? {
                    columns[selected].width = value;
                }
                if selected != 0 {
                    if event.node == up.id() && selected > 1 {
                        columns.swap(selected, selected - 1);
                        selected -= 1;
                    } else if event.node == down.id() && selected + 1 < columns.len() {
                        columns.swap(selected, selected + 1);
                        selected += 1;
                    } else if event.node == show.id() || event.node == hide.id() {
                        columns[selected].visible = event.node == show.id();
                    }
                }
                sync(&columns, selected)?;
            }
            _ => {}
        }
    }
    Ok(None)
}
