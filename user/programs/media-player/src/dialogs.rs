//! 应用自己的小对话框：打开网址与媒体信息；控件均使用共享控件并由本函数持有到关闭。
use liteos_ui::{
    Column, Dialogs, EventKind, Layout, Widget, WidgetKind as Kind, WidgetSpec, Window,
    WindowOptions, state,
};

const APP_ID: &str = "org.liteos.media-player";

fn node(parent: &Widget, kind: Kind, text: &str, layout: Layout) -> liteos_ui::Result<Widget> {
    let mut spec = WidgetSpec::new(kind, text);
    spec.layout = layout;
    parent.child(spec)
}

fn is_escape(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Key {
            code: 1,
            pressed: true,
            ..
        }
    )
}

/// “打开网址”：返回用户输入的非空地址，取消返回 None。
pub async fn open_url(dialogs: &Dialogs, parent: &Window) -> liteos_ui::Result<Option<String>> {
    let mut options = WindowOptions::new("打开网址", APP_ID, 380, 120);
    options.fixed = true;
    options.modal = true;
    options.parent = Some(parent);
    let (window, mut events) = dialogs.window(options)?;
    let mut spec = WidgetSpec::new(Kind::Column, "");
    spec.layout = Layout {
        grow: 1,
        padding: 10,
        gap: 8,
        ..Default::default()
    };
    let root = window.widget(spec)?;
    let prompt = node(
        &root,
        Kind::Label,
        "请输入要播放的媒体文件或流的网址(&A):",
        Layout::default(),
    )?;
    let address = node(
        &root,
        Kind::Edit,
        "",
        Layout {
            height: 23,
            ..Default::default()
        },
    )?;
    let buttons = node(
        &root,
        Kind::Row,
        "",
        Layout {
            height: 23,
            gap: 6,
            ..Default::default()
        },
    )?;
    let spacer = node(
        &buttons,
        Kind::Label,
        "",
        Layout {
            grow: 1,
            ..Default::default()
        },
    )?;
    let ok = node(
        &buttons,
        Kind::Button,
        "确定",
        Layout {
            width: 75,
            ..Default::default()
        },
    )?;
    ok.set_state(state::DEFAULT)?;
    let cancel = node(
        &buttons,
        Kind::Button,
        "取消",
        Layout {
            width: 75,
            ..Default::default()
        },
    )?;
    address.focus()?;
    let _held = [&prompt, &spacer, &buttons];
    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close => return Ok(None),
            ref kind if is_escape(kind) => return Ok(None),
            EventKind::Activate { .. } if event.node == cancel.id() => return Ok(None),
            EventKind::Activate { .. } if event.node == ok.id() => {
                let text = address.text()?;
                let text = text.trim();
                if !text.is_empty() {
                    return Ok(Some(text.to_owned()));
                }
            }
            _ => {}
        }
    }
    Ok(None)
}

/// “媒体信息”：以两列列表显示 `(项目, 值)`，关闭后返回。
pub async fn media_info(
    dialogs: &Dialogs,
    parent: &Window,
    rows: &[(String, String)],
) -> liteos_ui::Result<()> {
    let mut options = WindowOptions::new("媒体信息", APP_ID, 460, 340);
    options.modal = true;
    options.parent = Some(parent);
    let (window, mut events) = dialogs.window(options)?;
    let mut spec = WidgetSpec::new(Kind::Column, "");
    spec.layout = Layout {
        grow: 1,
        padding: 8,
        gap: 8,
        ..Default::default()
    };
    let root = window.widget(spec)?;
    let list = node(
        &root,
        Kind::List,
        "",
        Layout {
            grow: 1,
            ..Default::default()
        },
    )?;
    list.set_state(state::CLIENT_EDGE)?;
    list.set_columns(&[
        Column {
            id: 1,
            title: "项目",
            width: 110,
        },
        Column {
            id: 2,
            title: "值",
            width: 320,
        },
    ])?;
    let lines: Vec<String> = rows
        .iter()
        .map(|(name, value)| format!("{name}\t{value}"))
        .collect();
    list.set_rows(
        &lines
            .iter()
            .enumerate()
            .map(|(index, line)| (index as u64 + 1, line.as_str()))
            .collect::<Vec<_>>(),
    )?;
    let buttons = node(
        &root,
        Kind::Row,
        "",
        Layout {
            height: 23,
            gap: 6,
            ..Default::default()
        },
    )?;
    let spacer = node(
        &buttons,
        Kind::Label,
        "",
        Layout {
            grow: 1,
            ..Default::default()
        },
    )?;
    let close = node(
        &buttons,
        Kind::Button,
        "关闭",
        Layout {
            width: 75,
            ..Default::default()
        },
    )?;
    close.set_state(state::DEFAULT)?;
    let _held = [&buttons, &spacer];
    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close => return Ok(()),
            ref kind if is_escape(kind) => return Ok(()),
            EventKind::Activate { .. } if event.node == close.id() => return Ok(()),
            _ => {}
        }
    }
    Ok(())
}
